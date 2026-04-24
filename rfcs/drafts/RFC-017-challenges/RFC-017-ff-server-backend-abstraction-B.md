# RFC-017-B: ff-server backend abstraction (ergonomics-first draft)

**Status:** Draft (divergent, author B)
**Author:** FlowFabric Team — Worker-B lane
**Created:** 2026-04-23
**Companion drafts:** RFC-017-A (author A, independent parallel draft). Consolidator merges A + B into RFC-017.
**Parent context:** `rfcs/drafts/v0.7-migration-master.md` §2 (inventory), §5 (stages), Part 4 Q1–Q15 (adjudicated design calls).
**Companion surveys:** `v0.7-migration-survey-engine.md` §4 (boot), §5 (route table), §6 (admin + obs).

---

## Summary

`ff-server` is the v0.7 migration's last Valkey-hardcoded surface. `EngineBackend` already owns the worker-facing write surface (~30 methods, `Arc<dyn>` object-safe). Every `Server::<method>` in `crates/ff-server/src/server.rs` still reaches directly into `ferriskey::Client` / `FCALL` / `HGET` — 23 raw call-sites counted in survey §5.2. This RFC proposes a **single-trait, single-`Arc<dyn>` refactor**: promote `EngineBackend` to cover everything ff-server does in normal steady state, model the three genuine non-steady-state concerns (stream tails, admin rotate, boot) as **first-class trait methods**, and turn `Server` into a thin HTTP adapter over one `Arc<dyn EngineBackend>`.

Author B's position, stated up-front and argued throughout:

1. **One trait, not many.** Expand `EngineBackend` to ~42 methods. Do not split into `IngressBackend` / `AdminBackend` / `StreamBackend`. One trait is easier to implement, easier to mock, and easier to reason about than four traits with subset-relationships.
2. **`Arc<dyn EngineBackend>` everywhere in `Server`.** No concrete backend enum. No type parameter. One field, one vtable hop per handler, zero monomorphised copies of the 1.5k-line server.
3. **Valkey primitives live in `ff-backend-valkey`, always.** `stream_semaphore` / `xread_block_lock` / `tail_client` are Valkey-impl details and belong *inside* the Valkey backend, not on the `Server` struct. Postgres gets a trivial impl; mocks get nothing.
4. **`Server::start(config)` stays; a new `start_with_backend(backend)` lands alongside.** The string-url boot path keeps running for v0.7.0 consumers; the backend-injection path is what cairn + tests use.

§10 (alternatives) documents where I diverge from the migration-master lean and why.

---

## Motivation

§5.2 of the engine survey puts it plainly: **"All Valkey call-sites concentrate in `server.rs`; the HTTP-to-backend seam is `Server::<method>()`."** That seam is already the right abstraction boundary — handlers in `api.rs` are thin serde wrappers that call `self.server.<method>(...)`. The missing piece is that `Server::<method>` talks to ferriskey, not to a trait.

Three concrete pains today:

- **No Postgres over HTTP.** v0.7's Postgres backend lands as a library (`ff-backend-postgres` implementing `EngineBackend`) but consumers only reach FlowFabric via `ff-server`'s REST API. A Postgres-backed cairn deployment cannot use ff-server without this RFC.
- **No mockable handler tests.** `ff-server` unit tests spin up a real Valkey container. The backend trait is the natural seam for `MockBackend`; `Server` blocks it.
- **Leaky abstractions.** `Server::rotate_waitpoint_secret` already knows about `Partition` + `num_flow_partitions` + `ff_rotate_waitpoint_hmac_secret` FCALL name. The HTTP layer has no business knowing any of those.

The ergonomic bet of this RFC: **one Arc, one trait, expanded to cover what ff-server actually calls.** The alternative — sharding the trait, or keeping a concrete backend enum — saves nothing and costs churn every time a new handler or backend lands.

---

## Non-goals

- Changing `EngineBackend`'s existing method signatures. Round-7 stabilised them; this RFC only **adds** methods.
- Migrating the HTTP wire format. `api.rs` stays byte-for-byte compatible.
- Removing the Valkey boot dance from `Server::start` (steps 1b / 2 / 2b / 3 / 3b in survey §4). Those move behind the backend trait but continue to run on `BackendKind::Valkey` deployments.
- Dual-backend per-process running (migration-master §8 already rules that out).
- Replacing the completion listener's `CompletionBackend` trait (separate surface; this RFC touches write-surface only, reads `CompletionStream` the same way `Engine` does).

---

## Detailed design

### §3. Trait scope: one trait, 42 methods

Today: `EngineBackend` = 30 methods (ingress-facing, per RFC-012 §3.1).

Proposed: `EngineBackend` = **42 methods**. The 12 additions cover every ingress + admin op `ff-server` currently serves via raw FCALL. Full list, grouped by family; bold are new in RFC-017-B:

**Claim + lifecycle (11, unchanged):** `claim`, `renew`, `progress`, `append_frame`, `complete`, `fail`, `cancel`, `suspend`, `create_waitpoint`, `observe_signals`, `claim_from_reclaim`, `delay`, `wait_children`.

**Reads (8, unchanged):** `describe_execution`, `describe_flow`, `list_edges`, `describe_edge`, `resolve_execution_flow_id`, `list_flows`, `list_lanes`, `list_suspended`, `list_executions`.

**Trigger (3, unchanged):** `deliver_signal`, `claim_resumed_execution`, `cancel_flow`.

**Budget + edges (2, unchanged):** `set_edge_group_policy`, `report_usage`.

**Streams (3, unchanged, feature-gated):** `read_stream`, `tail_stream`, `read_summary`.

**HMAC rotation (1, unchanged, feature-gated):** `rotate_waitpoint_hmac_secret_all`.

**Ingress writes (NEW, 8):**

- **`create_execution(CreateExecutionArgs) -> CreateExecutionResult`** — today called via raw `FCALL ff_create_execution`. Moves to the trait so Postgres serves the same shape.
- **`create_flow(CreateFlowArgs) -> CreateFlowResult`**
- **`add_execution_to_flow(AddExecutionToFlowArgs) -> AddExecutionToFlowResult`**
- **`stage_dependency_edge(StageDependencyEdgeArgs) -> StageDependencyEdgeResult`**
- **`apply_dependency_to_child(ApplyDependencyToChildArgs) -> ApplyDependencyToChildResult`**
- **`cancel_execution(CancelExecutionArgs) -> CancelExecutionResult`** — operator-initiated (not worker-cooperative `cancel`).
- **`change_priority(ChangePriorityArgs) -> ChangePriorityResult`**
- **`replay_execution(ReplayArgs) -> ReplayResult`**

**Budget/quota admin (NEW, 4):**

- **`create_budget(CreateBudgetArgs)`**
- **`reset_budget(BudgetId)`**
- **`create_quota_policy(CreateQuotaPolicyArgs)`**
- **`get_budget_status(BudgetId) -> BudgetStatus`**

**Diagnostics (NEW, 1):**

- **`ping() -> ()`** — `/healthz` uses it; Valkey impl is `PING`, Postgres is `SELECT 1`. Trivial, but removes the last direct ferriskey call from `api.rs`.

**Lease admin (NEW, 1):**

- **`revoke_lease(RevokeLeaseArgs) -> RevokeLeaseResult`**

**Operator-facing pending-waitpoint listing (NEW, 1):**

- **`list_pending_waitpoints(ExecutionId) -> Vec<PendingWaitpointSnapshot>`** — today read via HMGET in `Server`; the Valkey impl keeps that, Postgres reads one row.

**Boot (NEW, wrapped; not on the trait itself — see §5):** `validate_partition_config`, `ensure_library_loaded`, `seed_lanes`, `initialize_waitpoint_hmac_secret`, `verify_min_version` live on a companion `BackendBoot` trait. Rationale in §5.

**Total on `EngineBackend`: 42 methods.** Total backend-facing surface counting boot: 47.

#### §3.1 Why one trait and not three

The seductive split is `IngressBackend` (writes) + `EngineBackend` (worker surface) + `AdminBackend` (rotate/reset). I reject it. Three reasons:

1. **Consumers always want all three.** `ff-server` is the only non-test consumer of ingress ops; ff-sdk is the only consumer of worker ops; migration-tool is the only consumer of admin ops. But in every case the backing concrete type (`ValkeyBackend`, `PostgresBackend`) implements all three. Splitting the trait would mean three `Arc<dyn>` fields on `Server`, each pointing at the same `Arc<ValkeyBackend>` — pure ceremony.
2. **Migration-master Q13 (SDK version-pin) already locks us into additive-only evolution.** A split buys us "a tiny backend could implement only worker ops" — but v0.7 has two concrete backends and zero use cases for a half-backend. Speculative flexibility.
3. **Object-safety is easier to maintain on one trait.** The `_assert_dyn_compatible` guard at `engine_backend.rs:663` is one compile-time check. Three traits = three checks, three chances to regress.

A senior engineer reading 42 `async fn` on one trait will ask "why is this trait so big?" The answer is **because the backend is one component and it does these 42 things**. The split is the speculative code; the single trait is the honest shape.

#### §3.2 Downcasting is banned

A tempting escape hatch: keep `EngineBackend` minimal and `Any`-downcast to `ValkeyBackend` for the "extra" 12 methods. I **reject** this. Downcasting hides backend coupling in string-literal form, defeats mocking for handler tests, and means a Postgres-backed ff-server silently 500s on the downcasted path rather than compile-erroring.

**The trait is the contract. If a method is on the trait, every backend implements it. If a method isn't on the trait, no consumer calls it.**

### §4. `Server` refactor: one `Arc<dyn EngineBackend>`, no enum, no generic

Current `Server` has 12 fields. The proposal deletes 4 and replaces 2:

```rust
pub struct Server {
    // ── replaced ──
    backend: Arc<dyn EngineBackend>,          // was: client: Client
    // ── deleted (move into ff-backend-valkey) ──
    // tail_client: Client,                   // Valkey-specific
    // stream_semaphore: Arc<Semaphore>,      // Valkey-specific
    // xread_block_lock: Arc<Mutex<()>>,      // Valkey-specific
    // ── kept (backend-agnostic) ──
    admin_rotate_semaphore: Arc<Semaphore>,   // server-wide fairness gate
    engine: Engine,
    config: ServerConfig,
    scheduler: Arc<ff_scheduler::Scheduler>,
    background_tasks: Arc<AsyncMutex<JoinSet<()>>>,
    metrics: Arc<ff_observability::Metrics>,
}
```

Every `Server::<method>` handler now reads:

```rust
pub async fn create_execution(
    &self,
    args: &CreateExecutionArgs,
) -> Result<CreateExecutionResult, ServerError> {
    self.backend.create_execution(args.clone()).await.map_err(ServerError::from)
}
```

That is the entire v0.7 handler. The 1.5k-line server file collapses to ~400 lines — the remaining lines are the boot sequence (§5), the rotation fan-out wrapper (§6), and the Engine plumbing.

#### §4.1 Why not a concrete enum `Backend { Valkey(...), Postgres(...) }`

Rejected. Three reasons:

1. **Every handler becomes a `match`.** 13 handlers × 2 variants = 26 arms, every one just calls the method. Noise with no semantic content.
2. **Third-party backends lose.** A test `MockBackend` or a future cloud backend (SQLite? DynamoDB?) has to be added to the enum in the ff-server crate. That's a dependency cycle in the wrong direction.
3. **Match dispatch is not faster than vtable dispatch** at this call frequency (dozens per second per handler, not millions). The optimisation is imaginary.

#### §4.2 Why not a type parameter `Server<B: EngineBackend>`

Rejected harder. Three reasons:

1. **Monomorphisation explosion.** `Server<ValkeyBackend>` and `Server<PostgresBackend>` each generate a full copy of the 1.5k lines of handler code. At two backends today that's +400KB of binary; with migration-tool test mocks it's worse.
2. **Heterogeneous-backend binaries break.** A future `ff-server-multi` binary that can serve both Valkey and Postgres tenants from one process (even if v0.7 rules that out — migration-master §8.1) becomes a generic hell.
3. **`axum`'s `State<Arc<Server>>` extractor doesn't care.** The handler type is `Arc<Server>`, not `Arc<Server<B>>`. Making it generic forces every `router()` caller to specialise.

`Arc<dyn EngineBackend>` is the right answer. The overhead is one pointer-chase per call; `ferriskey::Client` already costs more than that per call.

### §5. Boot sequence: `BackendBoot` companion trait

Survey §4 lists eight boot steps; five are Valkey-specific (1, 1b, 2, 2b, 3, 3b) and three are backend-agnostic. Putting boot on `EngineBackend` itself is wrong — boot happens **before** `EngineBackend` is usable, and test mocks don't want to implement a `verify_min_version` they don't have.

Proposed shape:

```rust
#[async_trait]
pub trait BackendBoot: Send + Sync {
    /// Connect + verify + install schema/functions. Returns a ready-to-use
    /// `Arc<dyn EngineBackend>`. All Valkey-specific steps (version check,
    /// partition config, HMAC secret install, FUNCTION LOAD, lane seed)
    /// live inside the Valkey impl of this method.
    async fn boot(
        config: &BackendConfig,
        metrics: Arc<ff_observability::Metrics>,
    ) -> Result<Arc<dyn EngineBackend>, BackendBootError>;
}

pub enum BackendConfig {
    Valkey(ValkeyBackendConfig),    // host, port, tls, cluster, partition_config, lanes, hmac_secret, …
    Postgres(PostgresBackendConfig), // dsn, pool_size, schema, …
}
```

`Server::start(config: ServerConfig)` stays (see §7 for back-compat), but its body becomes:

```rust
pub async fn start_with_metrics(
    config: ServerConfig,
    metrics: Arc<ff_observability::Metrics>,
) -> Result<Self, ServerError> {
    let backend_config = config.backend.clone();  // BackendConfig
    let backend = match &backend_config {
        BackendConfig::Valkey(v)    => ValkeyBackend::boot(v, metrics.clone()).await?,
        BackendConfig::Postgres(p)  => PostgresBackend::boot(p, metrics.clone()).await?,
    };
    Self::start_with_backend(config, backend, metrics).await
}

pub async fn start_with_backend(
    config: ServerConfig,
    backend: Arc<dyn EngineBackend>,
    metrics: Arc<ff_observability::Metrics>,
) -> Result<Self, ServerError> {
    // ── backend-agnostic setup only ──
    let engine = Engine::start_with_backend(backend.clone(), &config.engine_config, metrics.clone()).await?;
    let scheduler = Arc::new(Scheduler::with_metrics(backend.clone(), &config.partition_config, metrics.clone()));
    Ok(Self { backend, engine, scheduler, ..default_fields() })
}
```

Two clean entry points. `start` takes a `ServerConfig` + URL-ish stuff, picks the backend from the config enum, and boots it. `start_with_backend` is the test-injection + future-embedded-user path. Both compile without a ferriskey dep if only Postgres is enabled (Cargo feature).

#### §5.1 Why not make `boot` a plain associated function on each backend crate

I.e. `ValkeyBackend::boot(...)` / `PostgresBackend::boot(...)` with no trait. That's actually fine too — `BackendBoot` is a convention, not load-bearing. But the trait version lets `Server` dispatch on the config enum with one match arm instead of a big if/else. Low cost, small win. Adopt.

### §6. Admin rotate path

Today: `Server::rotate_waitpoint_secret(new_kid, new_secret_hex)` does its own per-partition fan-out, parallelising with `BOOT_INIT_CONCURRENCY = 16`, emitting one aggregated audit event.

Proposed: **the fan-out moves inside `EngineBackend::rotate_waitpoint_hmac_secret_all`** (already the signature, already on the trait — v0.7 Q4 put it there). `Server::rotate_waitpoint_secret` becomes:

```rust
pub async fn rotate_waitpoint_secret(
    &self,
    new_kid: &str,
    new_secret_hex: &str,
) -> Result<RotateWaitpointSecretResult, ServerError> {
    let _permit = self.acquire_admin_rotate_permit()?;
    let args = RotateWaitpointHmacSecretAllArgs { new_kid, new_secret_hex };
    let result = self.backend.rotate_waitpoint_hmac_secret_all(args).await?;
    emit_aggregated_audit_event(&result);
    Ok(result.into())
}
```

The admin semaphore **stays on `Server`** — it's a server-wide fairness gate, not a backend concern. The audit emit stays on `Server` — it's an observability concern, already backend-agnostic per survey §6.3. The fan-out moves: the Valkey impl parallelises 256 partitions; the Postgres impl is one UPDATE. Trait consumer sees one call, gets one structured result.

### §7. Backward compatibility on `Server::start(config)`

**Full compat.** `Server::start(ServerConfig)` keeps its current signature. The pre-RFC `ServerConfig` exposes Valkey-shaped fields (`host`, `port`, `tls`, `cluster`, `partition_config`, `lanes`, `waitpoint_hmac_secret`, …).

Plan:

1. **`ServerConfig` grows a `backend: BackendConfig` field.** Default `BackendConfig::Valkey(ValkeyBackendConfig::from_legacy_fields(&self))` populated from the existing top-level fields via a From impl. No caller changes needed; a caller that doesn't touch `backend` gets Valkey.
2. **Existing top-level fields stay for one release.** `ServerConfig::host`, `::port`, etc. are `#[deprecated(since = "0.7.0", note = "use `.backend = BackendConfig::Valkey(…)` instead")]`.
3. **v0.8 removes the deprecated fields.** Callers update to the struct-with-enum form.

This mirrors how ff-sdk handled the ferriskey-`ClientBuilder` transition in the builder-pattern memo. Same playbook, works cleanly.

### §8. Per-handler migration table

13 HTTP handlers, categorised by migration difficulty. "Trivial" = one-line delegate to an existing trait method. "Moderate" = adds a new trait method but the shape is obvious. "Hard" = requires a wire-type design call before the trait method can be spec'd.

| # | Handler | Difficulty | Trait method | Notes |
|---|---|---|---|---|
| 1 | `healthz` | Trivial | `ping` (NEW) | Only direct Valkey call in `api.rs` today; see §3 ping entry. |
| 2 | `list_executions` | Trivial | `list_executions` (existing) | Already on the trait. Delegates verbatim. |
| 3 | `get_execution` / `get_execution_state` / `get_execution_result` | Trivial | `describe_execution` / variants | All three already on trait; `get_execution_result` reads a payload blob — consider adding `read_result(ExecutionId)` or extending `ExecutionSnapshot` to include the result bytes. §9 Q3. |
| 4 | `cancel_execution` | Moderate | `cancel_execution` (NEW) | Operator-initiated cancel; wire type already exists (`CancelExecutionArgs`). |
| 5 | `change_priority` | Moderate | `change_priority` (NEW) | `ChangePriorityResult` needs a public core type. |
| 6 | `replay_execution` | Moderate | `replay_execution` (NEW) | `ReplayArgs` / `ReplayResult` exist; promote to public. |
| 7 | `revoke_lease` | Moderate | `revoke_lease` (NEW) | Straightforward trait add; parsing already backend-agnostic. |
| 8 | `deliver_signal` | Trivial | `deliver_signal` (existing) | Already on the trait (issue #150). One-line delegate. |
| 9 | `list_pending_waitpoints` | Moderate | `list_pending_waitpoints` (NEW) | Wire type needed; HMAC token surface in the response is sensitive — see §9 Q1. |
| 10 | `claim_for_worker` | Trivial | (delegates to `Scheduler`) | Scheduler holds its own `Arc<dyn EngineBackend>` (v0.7 Stage-C scope, not this RFC). `Server` forwards. |
| 11 | `read_attempt_stream` / `tail_attempt_stream` | Trivial | `read_stream` / `tail_stream` (existing) | Already on trait. Tail_client + semaphore + mutex move INTO `ValkeyBackend` per §9.4. |
| 12 | `create_flow` / `add_execution_to_flow` / `cancel_flow` / `stage_dependency_edge` / `apply_dependency_to_child` | Moderate | `create_flow` / `add_execution_to_flow` / `cancel_flow` (existing) / `stage_dependency_edge` (NEW) / `apply_dependency_to_child` (NEW) | `cancel_flow` already on trait. Edge ops need promotion. |
| 13 | `create_execution` | **Hard** | `create_execution` (NEW) | Creates a full execution — the payload + policy JSON shape needs to survive the backend boundary. See §8.1. |
| 14 | `create_budget` / `get_budget_status` / `report_usage` / `reset_budget` / `create_quota_policy` | Moderate | all NEW except `report_usage` (existing) | Budget + quota admin; wire types are already public per ff-sdk. |
| 15 | `rotate_waitpoint_secret` | Moderate | `rotate_waitpoint_hmac_secret_all` (existing) | §6. |

Count: **1 Trivial-no-trait-change, 6 Trivial-existing-trait, 11 Moderate-new-trait, 1 Hard.**

#### §8.1 The only Hard handler: `create_execution`

It is "hard" only because today's `Server::create_execution` holds the KEYS + ARGV positional contract with `lua/execution.lua`. That is a **Valkey implementation detail** — the caller (the REST consumer) sends a `CreateExecutionArgs` struct and receives a `CreateExecutionResult`. The Lua positional dance is what `ValkeyBackend::create_execution` does internally. Move it.

The design call: **does `CreateExecutionArgs` survive the trait boundary unchanged?** Answer: yes. Its fields are all public-contract domain types (`ExecutionId`, `LaneId`, `Namespace`, `Priority`, `Tags`, `InputPayload`, `ExecutionDeadlineMs`, `PartitionId`). The only Valkey-coupled thing is the `dedup_ttl_ms = 86400000` literal embedded at `server.rs:734` — which is today's idempotency TTL. Lift it to `CreateExecutionArgs::idempotency_ttl: Option<Duration>` (default 24h). Zero wire impact.

Upgraded from Hard to Moderate after thinking about it for 10 minutes.

### §9. Valkey-specific primitives: where they live after the refactor

Survey §5 identified three Valkey-only fields on today's `Server`:

| Primitive | Today | After RFC-017-B | Rationale |
|---|---|---|---|
| `tail_client: Client` | `Server` field | **Private field of `ValkeyBackend`** | It's a dedicated second ferriskey connection. Postgres backend doesn't need it. Move it behind the trait; `tail_stream` impl uses it internally. |
| `stream_semaphore: Arc<Semaphore>` | `Server` field | **Private field of `ValkeyBackend`** | Exists because `ferriskey::Client` is pipelined — a Valkey property. Postgres's `tail_stream` impl might want a different concurrency gate (or none). Move it. |
| `xread_block_lock: Arc<Mutex<()>>` | `Server` field | **Private field of `ValkeyBackend`** | Exists because `XREAD BLOCK` serialises on a pipelined conn (survey §5.2 + server.rs:157 comment). Postgres's LISTEN semantics don't need it. Move it. |
| `admin_rotate_semaphore: Arc<Semaphore>` | `Server` field | **Stays on `Server`** | Server-wide fairness gate, not backend-coupled. Applies equally to Postgres. |
| Scanner loops / `Engine` | `Server::engine` | **Stays on `Server`** (unchanged) | Already takes `Arc<dyn CompletionBackend>` + `Arc<dyn EngineBackend>` per RFC-012. |

**Back-pressure on stream ops moves with the primitive.** Today, `ConcurrencyLimitExceeded("stream_ops", 64)` surfaces as HTTP 429 when the semaphore denies a permit. After the move, `ValkeyBackend::tail_stream` returns `EngineError::ResourceExhausted { pool: "stream_ops", max: 64 }` and the mapping layer in `ServerError::from` converts to 429. Error kind is already on `BackendErrorKind` (issue #88). No new error variant needed.

#### §9.1 What if a future backend wants its own concurrency gate

Fine. Each backend's `tail_stream` impl is free to instantiate its own. The RFC does **not** prescribe a gate at the trait level — that's a speculative abstraction. When a second backend needs a gate and they look identical, we can extract; not before.

### §10. Alternatives considered (disagreements with the consolidator's likely lean)

The migration-master §10 open-questions list + survey §7 lean toward a more conservative shape. I disagree on three points.

#### §10.1 Master-spec leans toward minimal trait + downcasting

The adjudicated Q1 outcome (completion transport) left room for "keep the FCALL, introduce a narrow trait just for dependency resolution." Applied to the ingress surface, that's "keep `EngineBackend` at 30 methods, downcast in `Server` for the extra 12." I reject it. See §3.2 — downcasting breaks the mock path and silently regresses Postgres parity.

#### §10.2 Master-spec is open to a backend enum

§8.1 of the migration-master talks about dual-backend support at the per-instance level as "per ff-server instance targets one backend." That wording is compatible with either `Arc<dyn>` or a `Backend` enum. I pick `Arc<dyn>`. §4.1 explains why; §4.2 explains why not a generic.

#### §10.3 Master-spec defers admin rotate

Q4 adjudicated 2026-04-24 put `rotate_waitpoint_hmac_secret_all` on the trait but did not say who does the 256-way fan-out. I am explicit: **fan-out is inside the backend**. See §6. A Postgres backend should not have to fake a fan-out just because `Server` expects to do one itself.

### §11. Stage plan

Four stages, CI-green at each boundary. Based on migration-master stages B/C/D/E but scoped to `ff-server` specifically.

**Stage 1 — Trait expansion + agnostic handler rewrite (weeks 0–2).**

Land the 12 new methods on `EngineBackend`; implement in `ValkeyBackend` (delegate to existing Lua FCALLs); implement in `PostgresBackend` as `unimplemented!()` behind a `#[cfg(feature = "postgres-stub")]` gate. Rewrite all 13 handlers to delegate through `self.backend.<method>(...)`. Keep `tail_client` / `stream_semaphore` / `xread_block_lock` on `Server` **temporarily** — stage 2 moves them. CI: `cargo test -p ff-server` against Valkey passes byte-for-byte.

CI gate: full workspace `cargo test`. The RFC-phase boundary must align with CI boundary per the pinned feedback memo — no "acceptance gate clean" before CI is green.

**Stage 2 — Primitive relocation (weeks 2–3).**

Move `tail_client` / `stream_semaphore` / `xread_block_lock` into `ValkeyBackend`. Update `ServerError` mapping so `EngineError::ResourceExhausted { pool: "stream_ops" }` → HTTP 429. Delete the three fields from `Server`. CI: full workspace, no regressions; 429 semantics verified in integration test.

**Stage 3 — `BackendBoot` + config enum (weeks 3–5).**

Land `BackendBoot` trait. Add `ServerConfig.backend: BackendConfig`. Keep legacy Valkey-flat fields with `#[deprecated]`. Implement `PostgresBackend::boot` and a smoke test that runs `Server` against a Postgres backend with a mock `create_execution` / `claim` path. CI: two-backend matrix job added (Valkey + Postgres stub).

**Stage 4 — Postgres parity hardening (weeks 5–9).**

Implement the full 42 `EngineBackend` methods in `PostgresBackend`. Smoke against a published ff-server binary using the "release publish-list drift" rule (serialize CHANGELOG; smoke against published artifact). No scope creep; if a method has a subtle semantic that Valkey got for free from Lua atomicity, document the SQL transaction pattern alongside.

**Stage 5 — Deprecation cleanup (v0.8, weeks 10+).**

Remove deprecated flat-field `ServerConfig` entries. Remove the `postgres-stub` gate. Publish the migration matrix to the cairn peer team per the peer-team-boundaries rule.

Stage 1 is the load-bearing stage. If it ships cleanly, 2/3/4 are mechanical.

### §12. Wire contract + error surface

Zero wire-format changes. Every trait method's Args / Result types are already public in `ff-core::contracts` or promoted to public by this RFC. No new error variants on `ServerError`; the existing `Backend(BackendError)` / `BackendContext` variants absorb `EngineError` via a `From<EngineError> for ServerError` impl that's a straightforward mapping across all `EngineError` variants.

`EngineError::Unavailable { op }` (the default impl fallback for `rotate_waitpoint_hmac_secret_all`) surfaces as `ServerError::OperationFailed(format!("op {op} not supported on this backend"))` → HTTP 501. Loud not silent.

### §13. Testing strategy

- **`MockBackend`** struct lands in `ff-server`'s test module. Implements every `EngineBackend` method with `Arc<Mutex<ScriptedResponses>>`. Handler tests run without a Valkey container. **This is the single biggest ergonomic win from the RFC.**
- **Parity test matrix.** `ff-server`'s integration tests parametrize on `BackendKind::{Valkey, Postgres}`. Every handler runs against both.
- **Scratch-project smoke after publish.** Per the pinned "smoke after publish" memo: build a minimal `ff-server` binary against the published `ff-backend-postgres` crate and hit all 13 handlers end-to-end before closing the release.

### §14. Observability

- `ServerError::backend_kind()` already returns `ff_core::BackendErrorKind` (issue #88). Keep. Metrics label backend kind via `Arc<dyn EngineBackend>::backend_label() -> &'static str` (new trait method — always-there default returns `"unknown"`, concrete backends override).
- The aggregated audit event for rotation stays on `Server` (§6). `tracing::info!(target: "audit", …)` format unchanged.
- No new metrics added by this RFC. `ff_stream_ops_concurrency_rejected` counter relocates from `Server` to `ValkeyBackend`; same metric name, same label, same scrape.

---

## Top open questions (for consolidator)

1. **Do we accept `backend_label() -> &'static str` on the trait?** Needed for metrics dimensioning under dual-backend fleets. Trivial to add but adds one more method (making it 43). Author B: yes, add it.
2. **Does `get_execution_result` extend `ExecutionSnapshot` or get its own method?** §8 row 3. Author B: extend, snapshot already has Option<ResultBytes> shape — cheaper than a new method.
3. **Does `list_pending_waitpoints` expose HMAC tokens in the response shape?** Today it does (survey §5.1). That's a security surface, not just a wire-shape surface. Author B: keep current response, but move the token-signing step inside the backend — do not serialize unsigned tokens across the trait boundary.
4. **Is stage 4 a v0.7.x minor or a v0.8.0?** Removal of deprecated `ServerConfig` flat fields breaks semver. Author B: v0.8.0.
5. **Do we allow a third-party crate to implement `BackendBoot`?** I.e. can a user ship their own backend? Author B: yes, trait is `pub`, no sealed-trait pattern. Postgres-and-Valkey-only is speculative lock-in.

---

## Position summary (for consolidator merge)

- **Trait scope:** expand `EngineBackend` to 42 methods. One trait, no split, no downcasting.
- **Server struct:** `Arc<dyn EngineBackend>` singleton field. No enum, no generic. Delete three Valkey-specific primitives from `Server`.
- **Valkey primitives:** inside `ff-backend-valkey::ValkeyBackend`. Stream back-pressure surfaces as `EngineError::ResourceExhausted` mapped to HTTP 429.
- **Admin rotate:** fan-out inside the backend (Valkey: 256 partitions; Postgres: one UPDATE). Semaphore + audit emit stay on `Server`.
- **Handler table:** 7 Trivial, 5 Moderate, 0 Hard (after §8.1 re-analysis).
- **Back-compat:** `Server::start(config)` kept; `ServerConfig.backend` added with a From impl from legacy flat fields; deprecated in v0.7.0, removed in v0.8.0.
- **Stage plan:** 4 stages + deprecation cleanup; weeks 0–9 for the mechanical work + weeks 10+ for v0.8 cleanup.

Divergences from the migration-master lean: (a) reject downcasting, (b) reject backend enum, (c) admin fan-out inside backend.
