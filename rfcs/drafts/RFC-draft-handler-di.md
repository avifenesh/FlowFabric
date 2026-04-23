# RFC-draft: Handler DI / extractor pattern for FlowFabric consumer workers

**Status:** Draft (investigation + design)
**Author:** Worker VVVV
**Created:** 2026-04-22
**Related:**
- `rfcs/drafts/apalis-deep-architecture-dive.md` (Worker PPPP, §Handler DI rec #4)
- `rfcs/drafts/apalis-comparison.md` (Worker VV)
- `rfcs/drafts/RFC-draft-tower-layer-surface.md` (Worker UUUU, if landed — orthogonal client-local layer surface)
- RFC-012 §Stage 1d (higher-op trait work — `suspend` input-shape rework + `ConditionMatcher`↔`WaitpointSpec`)
- Issue #135 (higher-op trait scope)

---

## §1 Motivation

### §1.1 DX gap today

FlowFabric consumers write an explicit imperative worker loop:

```rust
let worker = FlowFabricWorker::connect(cfg).await?;
let state = Arc::new(Pool::new(...));
loop {
    let Some(task) = worker.claim_next().await? else {
        tokio::time::sleep(idle_backoff).await;
        continue;
    };
    let state = Arc::clone(&state);
    tokio::spawn(async move {
        match do_work(task, state).await {
            Ok(out) => { let _ = task.complete(out).await; }
            Err(e)  => { let _ = task.fail(classify(&e), &e.to_string()).await; }
        }
    });
}
```

Every consumer reinvents: idle-backoff, spawn policy, panic catch, classify-on-error, retry-vs-fatal, shared-state threading. Worker PPPP's deep-dive ranked this the **#2 consumer-impact gap** vs apalis
(`apalis-deep-architecture-dive.md:226-230`). This RFC proposes closing it.

### §1.2 apalis precedent

apalis lets the consumer write:

```rust
async fn send_email(task: Email, data: Data<PgPool>, ctx: WorkerContext) -> Result<(), Error> { ... }
WorkerBuilder::new("emails").data(pool).backend(storage).build(send_email).run().await;
```

`FromRequest` implementations (`apalis-core/src/task_fn/`) resolve each argument from the task envelope. Up to 16 extractor args, tuple-macro-generated. Axum-flavour; low friction; the pattern is broadly liked.

### §1.3 Why now

Three enablers align:

1. **RFC-012 Stage 1b landed** (`6f54f9b`, PR #119). `ClaimedTask` is a thin forwarder over the `EngineBackend` trait. Adding a runtime shell over it is a superficial layer, not an engine change.
2. **Higher-op trait work (#135)** is on the runway. DI-runtime sits naturally **above** higher-op; shipping DI before higher-op freezes a handler shape that higher-op will have to thread around.
3. **Owner lock 2026-04-22** made 0.4.0 a Cairn-asks gate; 0.5.0 is DX-polish-friendly.

Recommendation (previewed): draft signature now, stage the runtime **after** higher-op lands (0.5.0), keep imperative loop as first-class forever.

---

## §2 Proposed handler signature + runtime

### §2.1 Handler

Multi-arg, extractor-based, axum/apalis-flavour:

```rust
async fn handler<E: ExecInput>(
    task: ClaimedTask<E>,
    state: Data<AppState>,
    ctx: WorkerContext,
) -> Result<CompleteOutput, HandlerError>;
```

- **First positional arg** is the task itself (`ClaimedTask<E>`). Not an extractor — it is the anchor; the runtime's `.handler(f)` is generic over this.
- **Subsequent args** implement `FromTask` (see §3), unbounded in count (tuple-macro up to 16, matching apalis).
- **Return** is `Result<Out, HandlerError>` where `Out: IntoCompleteOutput` (blanket-impl for `()`, `serde_json::Value`, `Vec<u8>`, plus user types with a derive). `HandlerError` classifies into `EngineError`-compatible buckets (`Retry`, `Fatal`, `Defer(Duration)`, `WaitChildren`).

### §2.2 Runtime

```rust
WorkerRuntime::new(worker)
    .with_state(pool)                  // Data<PgPool>
    .with_state(metrics)               // Data<MetricsClient>
    .concurrency(16)                   // local semaphore, not engine fence
    .catch_panic(true)                 // default on
    .idle_backoff(Duration::from_millis(50)..Duration::from_secs(2))
    .handler(send_email)
    .run(shutdown_token).await?;
```

Runtime owns:

- The `claim_next`/`claim_from_reclaim` loop.
- Idle-backoff (exponential, capped).
- A local semaphore for concurrency.
- `spawn` per claimed task on the ambient runtime.
- `catch_unwind` wrap → `ClaimedTask::fail(FailureKind::Panic, ...)`.
- Mapping `Result<Out, HandlerError>` → `complete` / `fail` / `delay_execution` / `move_to_waiting_children`.
- Graceful shutdown: on `CancellationToken` fire, stop claiming, drain in-flight to completion or lease-expiry.

Runtime does **not** own:

- Retry policy (engine-enforced via `ExecutionPolicy`, RFC-003). A handler returning `HandlerError::Retry` just `fail`s with a retryable classification; the engine decides re-enqueue.
- Timeout (engine-enforced via lease TTL). A client-local timeout would bypass the fence triple.
- Tracing/metrics — those compose via the tower-layer surface (RFC-draft-tower-layer-surface), not as runtime responsibilities.

### §2.3 Both paths coexist (forever)

The imperative loop stays first-class. Rationale:

- **Advanced use cases** (manual pacing, custom batching, multi-claim patterns, stream-tailing mixed with task-claiming) want the explicit loop.
- **Internal crates** (`ff-engine`, `cairn-fabric`-side) should not depend on the runtime shell.
- Runtime is a thin convenience over the same public API the imperative loop uses; no behaviour only reachable through `WorkerRuntime`.

`WorkerRuntime` is a strict addition, not a replacement.

---

## §3 Extractors

### §3.1 `FromTask` trait

```rust
pub trait FromTask<E: ExecInput>: Sized {
    type Rejection: Into<HandlerError>;
    fn from_task(task: &ClaimedTask<E>, ctx: &RuntimeCtx) -> Result<Self, Self::Rejection>;
}
```

Synchronous (extractors don't do I/O; they pluck from already-claimed state). `RuntimeCtx` holds the per-runtime `Data` map + `WorkerContext`. Extractors failing → handler is not invoked; task is `fail`ed with `FailureKind::Validation`.

### §3.2 Built-in extractors

**Ship-in-0.5.0 set** (minimum useful):

1. **`Data<T: Clone + Send + Sync + 'static>`** — Arc-clone from the runtime's state map. Exactly apalis's shape. Missing state at runtime-build is a compile-time error via typed-builder or a run-start error (owner choice; see open Q1).
2. **`WorkerContext`** — worker_instance_id, lane set, partition, start time. Already-present data on the worker; extractor just clones.
3. **`FlowId` / `ExecutionId`** — thin wrappers that pull the two IDs off the `ClaimedTask`. Avoids `task.flow_id()` / `task.execution_id()` boilerplate inside handlers.
4. **`Namespace`** — pulls the RFC-012-amendment-namespace segment. Handlers that dispatch by namespace get it injected directly.
5. **`TracingCtx`** — the OTEL span context propagated via the task envelope (ff-observability bridge). Handlers opt in to continuing the engine-side span by accepting this extractor; otherwise a fresh span is fine.

**Deliberately NOT in 0.5.0:**

- `ExecutionPolicy` extractor — policy is engine-enforced, handler shouldn't branch on it client-side. Hot take; defer to user feedback.
- `Capabilities` extractor — routing is pre-claim; by handler time the match is already proven. Dead weight.
- `Payload<T: Deserialize>` — tempting, but execution input types are already typed via `ExecInput`. Adding a second deserialization layer is confusion.

### §3.3 Macro surface

`#[derive(FromTask)]` for composed extractor structs (pull multiple fields in one arg). Matches apalis + axum's macro; low priority, can land later.

---

## §4 What the runtime owns vs consumer owns

| Concern | Runtime owns | Consumer owns |
|---|---|---|
| Claim loop | yes | — |
| Idle backoff | yes (configurable) | — |
| Concurrency (local) | yes (semaphore) | — |
| Panic catch | yes (default-on) | can opt out |
| Spawn | yes (ambient tokio) | could override (advanced) |
| Retry decision | no (engine) | returns `HandlerError::Retry` |
| Timeout enforcement | no (engine lease TTL) | — |
| Tracing/metrics | no (tower layer) | — |
| State injection | yes (via `Data`) | declares `with_state` |
| Graceful shutdown | yes (via CancellationToken) | passes token |
| Terminal state (`complete`/`fail`) | yes (from return type) | returns `Result<Out, HandlerError>` |

The split mirrors apalis cleanly — but with the explicit engine-side delegation of retry/timeout/budget that apalis lacks.

---

## §5 Interaction with tower-layer surface (RFC-draft-tower-layer-surface)

If UUUU's tower-layer RFC lands, the two are **orthogonal axes, composed at different seams**:

- **Tower layers wrap the backend.** `EngineBackend` → `Layer<EngineBackend>` → wrapped backend. Concerns: client-local tracing spans on every FCALL, retry on transport errors, metrics counters on backend ops, panic catch at the backend-call granularity.
- **Handler DI wraps the worker loop.** `FlowFabricWorker` → `WorkerRuntime` → handler fn. Concerns: loop control, state injection, handler-return classification.

Both are present at once in a typical consumer:

```rust
let backend = ValkeyBackend::connect(...).await?
    .layer(TracingLayer::new())      // tower layer
    .layer(MetricsLayer::new());     // tower layer
let worker = FlowFabricWorker::connect_with(backend, cfg).await?;
WorkerRuntime::new(worker)
    .with_state(pool)                // DI
    .handler(send_email)             // DI
    .run(token).await?;
```

No overlap in responsibility; composition is clean. The layers see every backend op (including the `claim_next` that the runtime loop issues). The runtime sees task envelopes.

**Interwoven point** — one. Panic catch. Tower's `catch-panic` wraps backend ops; runtime's `catch_panic` wraps handler invocations. Different granularities; both justified; no conflict.

---

## §6 Interaction with higher-op trait (#135)

The higher-op trait layers **above** `EngineBackend` — batteries-included orchestration primitives (batch-claim, progress-report-with-checkpoint, saga-step). Stage-1d (RFC-012 amendment) reworks `suspend` input shape with typed `WaitpointSpec` + `ResumeCondition`.

Handler DI layers **above higher-op**:

```
ClaimedTask (thin forwarder, RFC-012 Stage 1b)
  └── FlowFabricWorker (Stage 1c)
        └── HigherOpWorker (#135 — optional trait, adds batch/saga)
              └── WorkerRuntime (this RFC — DI + loop)
```

Why DI sits above higher-op, not beside:

- Higher-op's methods (`batch_claim`, `progress_with_checkpoint`) are callable inside a DI handler body. No conflict.
- If higher-op changes `ClaimedTask` input shape (Stage 1d does this for `suspend`), extractors that reference it change shape too. Freezing DI before higher-op freezes commits us to breaking-change extractors at 0.6.0.
- DI runtime can `run(handler)` with a handler that uses **either** basic ops (`task.complete`) or higher-ops (`task.batch_complete(others)`); runtime is agnostic.

Net: DI is strictly additive on top of higher-op. **Ship higher-op first, DI second.**

---

## §7 Alternatives rejected

### §7.1 Async `FromTask`

Extractors could be `async fn from_task(...)`. Rejected: extractors pluck from already-claimed state; if they need I/O, they're not extractors, they're the handler body. Sync keeps error paths obvious.

### §7.2 Builder returns `impl Future`, drop `run()`

`WorkerRuntime::new(...).handler(f)` awaits directly. Rejected: breaks `.run(shutdown_token)` passing and config-then-run patterns. Explicit `.run()` matches apalis.

### §7.3 Handler-returns-`()` (panic or succeed)

Handler returns `()`; any Err is a panic. Rejected: conflates expected failure (`HandlerError::Fatal`) with bugs (panic). apalis got this right; copy it.

### §7.4 Handler DI without `with_state` — extract from backend

Pull `Data<T>` from backend state, not runtime state. Rejected: backend is domain-agnostic storage; injecting `PgPool` into it conflates layers. State belongs to the runtime.

### §7.5 Replace imperative loop entirely

`WorkerRuntime` is the only worker-driving path. Rejected: `ff-engine`, `cairn-fabric`-bridge, and advanced multi-claim use cases all need the imperative loop. Both paths coexist forever.

### §7.6 Separate `FromTask` vs `FromWorker` traits

Split state-plucking (`FromWorker`) from task-plucking (`FromTask`). Rejected: one trait with `&ClaimedTask` + `&RuntimeCtx` handles both. apalis got away with one; so can we.

---

## §8 Staging

### §8.1 Prereq: higher-op trait (#135) lands

Blocker. DI handler signature depends on the final `ClaimedTask` shape post-Stage-1d. Do not draft extractors against a shape that's still migrating.

### §8.2 ff-sdk-only, no engine change

Runtime + extractors are pure ff-sdk additions. No `ff-core` types. No Lua. No backend-trait changes. Feature-flagged (`runtime` feature on ff-sdk) initially; promote to default after one release of bake time.

### §8.3 Target release

**0.5.0 or 0.5.x.** Post-higher-op, pre-1.0. Not 0.4.0 (owner lock: Cairn-asks gate only).

### §8.4 Sequencing

1. Higher-op trait lands (RFC + impl).
2. This RFC → accepted.
3. Implementation PR (~2 weeks, single worker): `FromTask` trait + 5 built-in extractors + `WorkerRuntime` + example + tests.
4. 0.5.0 ships with runtime behind `runtime` feature flag.
5. One release cycle of bake.
6. 0.5.x promotes `runtime` to default feature. `#[derive(FromTask)]` macro lands.

### §8.5 Imperative loop stays

No deprecation. Ever. Two-surface forever.

---

## §9 Open questions for owner

1. **Missing-state policy.** Handler declares `Data<Foo>`; runtime built without `.with_state(foo)`. Compile-time error (typed-builder) or run-start error (hash-lookup)? apalis does run-start. Typed-builder is nicer but encumbers the runtime API with phantom-type gymnastics.

2. **Extractor failure → fail or skip?** Extractor returns `Err`. Default proposal: `task.fail(FailureKind::Validation, ...)`. Alternative: `task.delay_execution(backoff)` and let retry decide. Owner call.

3. **`WorkerRuntime` vs `WorkerBuilder` naming.** apalis calls it `WorkerBuilder` but the builder **is** the runtime (calls `.run()` on itself). Ours composes `FlowFabricWorker` + handler into a runtime; `WorkerRuntime` is more honest. Confirm.

4. **Spawn policy override.** Should the runtime accept a custom spawn function (for `tokio-uring`, `glommio`, or scope-local spawns)? apalis does. Low cost to add; high cost to retrofit. Recommend: yes, take a `Spawner` trait with a default tokio impl.

5. **Handler return → `IntoCompleteOutput` blanket.** Accepting `Result<(), HandlerError>` vs `Result<T: IntoCompleteOutput, HandlerError>`. The latter is more flexible but binds a new trait. Recommend: `IntoCompleteOutput` with blanket impls for `()`, `Value`, `Vec<u8>` and a derive for consumer types.

6. **Feature-flag name.** `runtime`? `handler-di`? `worker-runtime`? Low-stakes, but set at draft time.

---

## §10 Scope estimate

**Implementation** (post higher-op):

- `FromTask` trait + `RuntimeCtx`: ~100 lines.
- 5 built-in extractors: ~150 lines total.
- Tuple-macro for N-arg handlers (1..=16): ~300 lines (mostly boilerplate; steal shape from apalis MIT-licensed).
- `WorkerRuntime` + builder: ~400 lines.
- Integration with `FlowFabricWorker::claim_next` + shutdown: ~150 lines.
- Tests: ~600 lines (extractor unit tests, runtime loop integration tests, panic-catch, shutdown-drain).
- Example in `crates/ff-sdk/examples/`: ~80 lines.
- CHANGELOG + docs: ~100 lines.

**Total: ~1900 lines, single worker, ~2 weeks.** No engine change, no Lua, no wire format. Feature-flagged initial landing keeps blast radius small.

Review cost: one RFC round (this doc + one challenge), one implementation PR with one reviewer. Low-risk addition.

---

## §11 Citations

- apalis `FromRequest` + `TaskFn`: `apalis-core/src/task_fn/` (MIT, `geofmureithi/apalis` → redirects `apalis-dev/apalis`, `main` 2026-04-23)
- apalis `WorkerBuilder`: `apalis-core/src/worker/builder.rs`
- apalis `Data<T>` extractor: `apalis-core/src/task_fn/mod.rs`
- Worker PPPP deep-dive, §Handler DI: `rfcs/drafts/apalis-deep-architecture-dive.md:132-147` (rec #4 at `:292-297`)
- Worker VV comparison: `rfcs/drafts/apalis-comparison.md`
- FlowFabric imperative-loop baseline: `crates/ff-sdk/src/worker.rs:150-166`, claim loop shape in `crates/ff-sdk/examples/` (TODO: reference concrete examples)
- RFC-012 Stage 1d scope: `rfcs/RFC-012-engine-backend-trait.md:15-33`
- Tower-layer-surface RFC draft (if landed): `rfcs/drafts/RFC-draft-tower-layer-surface.md`
- Issue #135 higher-op trait scope: github.com/avifenesh/FlowFabric/issues/135

---

*Draft end. ~380 lines. No code changes, no implementation. Ready for round-1 challenge.*
