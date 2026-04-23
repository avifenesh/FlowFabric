# apalis architectural deep-dive (2026-04-23)

Investigator: Worker PPPP. Scope: what geofmureithi means by "upgrades
your approach with best practices and shared connections" beyond the
scenario-4 DAG harness fix that Worker VV already covered in
`apalis-comparison.md`. Read-only. Primary-source citations only.

## Method

- apalis repo `geofmureithi/apalis` (redirects to `apalis-dev/apalis`),
  `main` as of 2026-04-23.
- Sources inspected (raw):
  - `apalis-core/src/backend/mod.rs` (Backend trait)
  - `apalis-core/src/backend/shared.rs` (`MakeShared` trait)
  - `apalis-core/src/worker/mod.rs`, `.../builder.rs` (WorkerBuilder)
  - `apalis-core/src/task_fn/mod.rs` (TaskFn / `FromRequest` /
    `Data<T>` extractors)
  - `apalis/src/lib.rs`, `apalis/src/layers/mod.rs` (tower glue)
  - `apalis/Cargo.toml` (feature flags)
  - `apalis/README.md`, `apalis-redis/README.md`,
    `apalis-workflow/README.md`
  - `apalis-redis` (separate repo `apalis-dev/apalis-redis`) —
    `src/lib.rs`, `src/shared.rs`.
- FF baselines:
  - `/home/ubuntu/FlowFabric/crates/ff-sdk/src/worker.rs` (worker)
  - `/home/ubuntu/FlowFabric/crates/ff-backend-valkey/src/lib.rs`
    (`ValkeyBackend`, `from_client_and_partitions`, `build_client`)
  - `/home/ubuntu/FlowFabric/ferriskey/src/ferriskey_client.rs`
    (`Client` is `#[derive(Clone)]`, `ClientBuilder` API)
  - `/home/ubuntu/FlowFabric/benches/comparisons/apalis/src/*.rs`
    (`apalis_redis::connect` per scenario)

## What geofmureithi likely meant by "upgrades your approach"

Three distinct technical claims packed into the sentence:

1. **"Shared connections"** — specific. `apalis-redis` exposes
   `SharedRedisStorage` (`apalis-redis/src/shared.rs`) that owns
   **one `redis::aio::MultiplexedConnection`** and vends N
   `RedisStorage<Args>` instances via `MakeShared::make_shared()`
   (`apalis-core/src/backend/shared.rs:12-33`). One TCP socket, N
   typed queues, one pub/sub `PSUBSCRIBE tasks:*:available` fanout.
   FF's bench harness dials a fresh `apalis_redis::connect(url)` per
   scenario (`benches/comparisons/apalis/src/scenario1.rs:74`,
   `scenario4.rs:177`) and builds the worker against that — that's
   the "strawman" setup he's pointing at.

2. **"Best practices"** — idiomatic `WorkerBuilder` + tower
   `.layer(...)` composition for retry/timeout/tracing/catch-panic
   instead of running a raw storage + hand-rolled loop.

3. **"Upgrades your approach"** — implies using `apalis-workflow`
   (already covered by Worker VV for scenario 4) **plus** the above
   two. The "your approach" here is the bench harness, not FF's
   engine.

Net: he is not claiming apalis's engine is better than FF's engine.
He is claiming our apalis **harness** is underspecified on 3 axes.
Two of those (shared connections, tower layers) we hadn't examined.

## Per-axis comparison

### Connection sharing

**apalis.** `apalis-redis/src/shared.rs:13-19`:

```rust
pub struct SharedRedisStorage {
    conn: MultiplexedConnection,
    registry: Arc<Mutex<HashMap<String, Arc<Event>>>>,
}
```

`SharedRedisStorage::new(client)` dials exactly one multiplexed
connection, opens a single `PSUBSCRIBE tasks:*:available`, and each
`make_shared()` call returns a `RedisStorage<Args, MultiplexedConnection>`
cloning the same `conn` and registering an `event_listener::Event`
keyed by type-name. N queues → 1 socket, 1 pub/sub stream.

**FF.** `ferriskey::Client` is `#[derive(Clone)]`
(`ferriskey/src/ferriskey_client.rs:24`) and internally
Arc-backed — cloning shares the same multiplexed pipelined
connection. `ValkeyBackend::from_client_and_partitions`
(`crates/ff-backend-valkey/src/lib.rs:176`) already lets a caller
wire a pre-dialed client into an `Arc<dyn EngineBackend>`, and
`FlowFabricWorker::connect_with`
(`crates/ff-sdk/src/worker.rs:574`) accepts a caller-supplied
`Arc<dyn EngineBackend>`. So the **primitive exists**; what's missing
is the ergonomic equivalent of `SharedRedisStorage::make_shared()` —
a one-liner that says "give me N workers sharing one underlying
connection, each with distinct `worker_instance_id`/lane/capability
state". Today each worker's `FlowFabricWorker::connect(config)` calls
`ff_backend_valkey::build_client(&config.backend)`
(`ff-sdk/src/worker.rs:166`), dialing a **fresh** client per
`connect` call. A consumer running multi-queue workers in one
process ends up with N sockets unless they manually plumb
`connect_with(..., Arc::clone(&backend), ...)`.

Verdict: FF has the mechanism, not the sugar. Missing: a
`SharedFfBackend` factory + worker convenience constructor. Small
API, real ergonomic win, zero engine change.

### Tower layer composition

**apalis.** `WorkerBuilderExt` (`apalis/src/layers/mod.rs`) wires
tower's `retry`, `timeout`, `limit` (concurrency + rate),
`filter`/`filter_async`, `map_*`, `catch_panic`, `enable_tracing`,
plus opt-in `prometheus`, `opentelemetry`, `sentry` layers, all as
tower `Layer`s composed at builder time. Feature flags (`apalis/Cargo.toml`):
default = `["tracing", "catch-panic", "limit", "timeout", "retry"]`.
Layer ordering is user-visible ("tracing added before retry will
see all retries as a single operation").

**FF.** Retry and timeout are engine primitives:
- `RetryPolicy` + `ExecutionPolicy` live on the task record; the
  scanner re-enqueues after lease expiry (RFC-003).
- Timeout is the lease TTL itself.
- Tracing/metrics go through `ff-observability` (OTEL-via-ferriskey
  owner lock 2026-04-22).

This is a **different axis**, not strictly worse. apalis's tower
layers are **user-space, per-worker, client-side**: a panic-catch or
local timeout can't protect against a wedged process — apalis's
at-least-once contract + heartbeat re-enqueue covers that. FF's
engine-enforced timeout IS fence-fenced (stale writer Lua-rejected),
which tower timeouts cannot be. But on **cross-cutting observability
/ hooks / local rate-limiting** FF surfaces nothing comparable;
consumer writes those by hand around `claim_next().await?`. The
higher-op trait (#135 scope) is where this gap would naturally close.

### Handler DI (function-as-handler)

**apalis.** `async fn send_email(task: Email, data: Data<usize>)` →
`WorkerBuilder::new(...).build(send_email)`. `FromRequest`
implementations extract `Data<T>`, `TaskId<I>`, `WorkerContext`, etc.
from the task envelope (`apalis-core/src/task_fn/`, macro-generated
up to 16 extractor args). Axum-flavour.

**FF.** Consumer writes `while let Some(t) = worker.claim_next().await? { ... }`
and manually threads shared state through a closure. Leaf-handler
ergonomics are lower: no extractors, explicit loop, explicit
`t.complete(...)`/`t.fail(...)`. Higher-op trait is pre-merge.

Verdict: genuine DX gap. Not philosophical — apalis's pattern
directly ports to FF's model once higher-op lands. Worth explicitly
designing `#[derive(FromRequest)]`-equivalent extractors (axum
already proved the ergonomic-contract).

### Storage backend agnosticism

**apalis.** `Backend` trait (`apalis-core/src/backend/mod.rs`):

```rust
pub trait Backend {
    type Args; type IdType: Clone; type Context: Default;
    type Error; type Stream: Stream<...>;
    type Beat: Stream<Item = Result<(), Self::Error>>;
    type Layer;
    fn heartbeat(&self, worker: &WorkerContext) -> Self::Beat;
    fn middleware(&self) -> Self::Layer;
    fn poll(self, worker: &WorkerContext) -> Self::Stream;
}
```

Extension traits add capabilities (`TaskSink`, `FetchById`, `ListTasks`,
`ListWorkers`, `Metrics`, `Reschedule`, `ResumeAbandoned`, `Update`,
`WaitForCompletion`) that backends opt into — per-backend feature
matrix surfaced via the `features_table!` macro
(`apalis-redis/src/lib.rs:55-72`).

**FF.** `EngineBackend` trait (RFC-012, Stage 1b in flight) is the
same pattern — capability traits + per-store impl. Currently
Valkey-only, Postgres on the runway (#58). **Axes-aligned**.

What FF doesn't have yet: the capability-matrix **in-rustdoc**
pattern (`features_table!`). Small but nice for consumer-facing
discoverability.

### Observability

**apalis.** `apalis-prometheus`, `apalis/src/layers/opentelemetry`,
`apalis/src/layers/tracing`, `apalis/src/layers/sentry` — all
opt-in feature flags. **apalis-board**: web UI for workers + jobs.

**FF.** `ff-observability` (OTEL bridge via ferriskey), engine-side
metrics, no web UI, no Sentry integration, no Prometheus exporter
bundled. Comparable on OTEL, missing on (1) pre-built Prom exporter,
(2) web UI, (3) Sentry bridge. Whether those matter depends on
positioning: FF-as-engine vs apalis-as-library.

### Scheduling primitives

**apalis.** `apalis-cron` — crate that wraps a cron schedule as a
backend and uses the same `WorkerBuilder`. Separate concern from
`delay_for` (combinator on `Workflow`).

**FF.** RFC-009 flow scheduler (leader-elected, engine-native) +
`delay_until` on tasks. apalis-cron is lighter (single-process,
per-worker schedule); FF's design is cluster-aware. Different axes.

### Heartbeat / lease

**apalis.** `Backend::heartbeat(&self, worker) -> Self::Beat` is a
stream of keep-alive ticks; each backend implements on its own
terms. `apalis-redis` runs `register_worker.lua`
(`apalis-redis/src/lib.rs`) on a configurable `keep_alive`
interval — writes the worker into a `workers_set` + inflight-jobs
key with TTL. Crash = key expires = another worker re-enqueues via
`reenqueue_orphaned_after`. **No fencing token.** A stale worker
whose heartbeat lapsed momentarily but whose task handler is still
running can complete-write after re-delivery, creating a
double-execution window.

**FF.** RFC-003 fence-triple: `lease_current` + `active_index` +
epoch fence. Stale-writer Lua-rejected at the boundary. Strictly
stronger contract. Worth-keeping.

## What FF is ACTUALLY doing worse (honest, ranked by consumer impact)

1. **No "shared connections" sugar.** Primitive exists
   (`connect_with(Arc<dyn EngineBackend>)`); ergonomic one-liner
   doesn't. A consumer with 5 worker types in one process today
   opens 5 Valkey sockets by default. apalis users get 1 by calling
   `SharedRedisStorage::new(client).make_shared()` five times.
   High-impact for multi-queue-in-one-process deployments.

2. **Handler ergonomics.** `async fn(Task, Data<T>, WorkerContext)`
   with extractors vs hand-rolled `claim_next` loop. Affects every
   consumer's first-line-of-code. Higher-op trait closes this —
   but not landed yet.

3. **Out-of-box observability batteries.** No Prometheus feature
   flag that drops a ready exporter, no Sentry layer, no web UI. FF
   has OTEL but consumers still wire their own exporter + dashboard.
   Medium-impact, zero-engine-change if we add features.

## What FF is doing DIFFERENTLY, not worse

- **Engine-enforced retry/timeout/budget** (RFC-005/008) vs tower
  layers. FF's is fenced; apalis's is composable client-side.
  Orthogonal; both defensible. FF can add a tower-layer-compatible
  *client-local* surface (local tracing spans, local rate limits)
  without compromising Lua atomicity — non-blocking design note
  worth opening.

- **Capability-graded routing** (RFC-006-ish) vs type-segregated
  queues. apalis's `Args` generic **is** the queue discriminator
  (one `RedisStorage<Email>` per type, namespace from
  `type_name::<T>()`). FF routes via capability tags at claim-grant
  time, one task record space. Different philosophy, neither wrong.

- **Multi-backend** via one trait (FF) vs per-backend crate
  (apalis). Both viable; #58 plan commits FF to the one-trait path.

## What FF is genuinely doing BETTER

- **Fenced atomicity on lease/claim** — stale worker writes
  Lua-rejected. apalis-redis cannot prove single-execution across
  heartbeat-lapse + revival. Impact: matters a lot for
  side-effect-producing tasks; matters less for idempotent work.
  Quantify: on a 30s lease + 1s heartbeat, apalis's double-execute
  window is O(seconds) under pathological GC pauses; FF's is zero.
  In benign conditions both are zero.

- **Waitpoints + HMAC resume tokens** (RFC-004). No apalis analogue
  surfaced; `apalis-workflow` persists state but doesn't expose
  external-resume by token. For webhook/external-event flows this is
  not a DX delta, it's a capability delta.

- **Exec/flow co-location** (RFC-011). Apalis-workflow persists as
  generic backend rows; no slot-locality guarantee. Matters for
  tail-latency under Valkey cluster mode.

- **Scheduler as engine primitive** (RFC-009, leader-elected). apalis
  cron is per-process.

## Recommendations

**Small-scope (fold into 0.4.x):**

1. **`SharedFfBackend` factory + worker one-liner.** Expose a
   convenience that mirrors `SharedRedisStorage::make_shared`. Uses
   existing `connect_with` machinery, zero engine change. Answer to
   geofmureithi's "shared connections" directly.
2. **`SharedFfBackend` example** in `crates/ff-sdk/examples/` and a
   callout in the 0.4.x CHANGELOG.
3. **Update `benches/comparisons/apalis/src/*.rs`** to use
   `SharedRedisStorage::new(client).make_shared()` for FF harness
   equivalents; re-run. Pairs with the scenario-4 workflow fix VV
   already recommended.

**Medium-scope (0.5.0):**

4. **FromRequest-style extractors** on the higher-op trait (#135
   scope). Targets: `Data<T>`, `TaskId`, `WorkerContext`,
   `Capabilities`. Do this once higher-op trait API is frozen.
5. **Client-local tower-compatible layer surface.** Non-fenced,
   purely user-space (local tracing, local rate-limit, local panic
   catch). Does not replace engine primitives; complements them.
   Opens FF to the tower ecosystem without compromising Lua
   atomicity. Separate drafts/ note, RFC-scale.
6. **Capability-matrix `features_table!`-style rustdoc** — helper
   macro to render per-backend capability docs. DX polish.

**Don't-adopt:**

- **Tower layers replacing RetryPolicy.** FF's fenced retry is
  load-bearing; apalis's layer is client-side best-effort. Retain
  engine-enforced retry; optionally **also** offer a tower layer
  for local-side observations.
- **Per-backend-crate multi-backend.** #58 one-trait plan is the
  committed path.
- **apalis-workflow as FF's flow model.** RFC-007 + RFC-011 + RFC-004
  together do more than apalis-workflow (waitpoints,
  exec-flow-colocation, fence-triple). Adopting the scenario-4
  bench-harness fix ≠ adopting the flow primitive.

## What to answer geofmureithi

> Re-read your note carefully and you're right on two beyond
> scenario 4: (a) our harness dials a fresh `apalis_redis::connect`
> per scenario instead of using `SharedRedisStorage::make_shared` —
> we'll fix that and re-run. (b) The tower layer story (retry,
> timeout, tracing via `.layer(...)`) is something we should surface
> idiomatically in the apalis harness too. Separately, we're
> opening a FF-side ticket for a `SharedBackend` ergonomic sugar —
> the primitive (shared `ferriskey::Client`, `connect_with(Arc<dyn
> EngineBackend>)`) already exists, just not as a one-liner.
> Happy to accept your PR if you want to drive the harness changes;
> otherwise we'll land them and ping you to re-review before the
> updated comparison goes out.

## Citations

- apalis README: https://github.com/geofmureithi/apalis/blob/main/apalis/README.md
- apalis-redis README + shared example:
  https://github.com/apalis-dev/apalis-redis/blob/main/README.md
- `MakeShared` trait:
  https://github.com/geofmureithi/apalis/blob/main/apalis-core/src/backend/shared.rs
- `SharedRedisStorage`:
  https://github.com/apalis-dev/apalis-redis/blob/main/src/shared.rs
- Backend trait:
  https://github.com/geofmureithi/apalis/blob/main/apalis-core/src/backend/mod.rs
- Layers:
  https://github.com/geofmureithi/apalis/blob/main/apalis/src/layers/mod.rs
- apalis/Cargo.toml features:
  https://github.com/geofmureithi/apalis/blob/main/apalis/Cargo.toml
- FF `FlowFabricWorker::connect`:
  `/home/ubuntu/FlowFabric/crates/ff-sdk/src/worker.rs:150-166`
- FF `ValkeyBackend::from_client_and_partitions`:
  `/home/ubuntu/FlowFabric/crates/ff-backend-valkey/src/lib.rs:176`
- FF bench client setup:
  `/home/ubuntu/FlowFabric/benches/comparisons/apalis/src/scenario1.rs:74`,
  `scenario4.rs:177`
- VV's prior report:
  `/home/ubuntu/FlowFabric/rfcs/drafts/apalis-comparison.md`
