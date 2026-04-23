# RFC-draft: Client-local layer surface for `EngineBackend`

**Status:** Draft (Worker UUUU, 2026-04-22). Investigation + design only. No code in this spawn.
**Related:**
- `rfcs/drafts/apalis-deep-architecture-dive.md` (Worker PPPP, 2026-04-23) — recommendation #5.
- RFC-012 (`EngineBackend` trait).
- RFC-003 / RFC-004 / RFC-011 (fence-triple, lease, exec/flow co-location — invariants this RFC MUST NOT violate).
- apalis primary sources: `apalis/src/layers/mod.rs`, `apalis/Cargo.toml` (layer feature flags),
  tower ecosystem `tower::Layer` / `tower::Service`.

## §1 Motivation

Worker PPPP's apalis deep-dive (§"Tower layer composition", lines 104–129)
identified that apalis exposes a tower `Layer` surface for cross-cutting
concerns — retry, timeout, tracing, concurrency/rate limit, catch-panic,
prometheus, opentelemetry, sentry — all composed at `WorkerBuilder` time
via `.layer(...)`. Feature flags in `apalis/Cargo.toml` default-enable
`["tracing", "catch-panic", "limit", "timeout", "retry"]`.

FlowFabric has no comparable ergonomic surface. Cross-cutting concerns
today fall into two non-overlapping buckets:

- **Engine-enforced** (fenced, distributed-correct): `RetryPolicy` +
  `ExecutionPolicy` on the task record (RFC-005/008); lease TTL as
  timeout (RFC-003); flow budgets (RFC-008). These stay engine-side —
  a client-local layer cannot fence a stale writer.
- **Client-local** (user-space, per-worker): local tracing spans around
  a trait call, in-process rate caps, panic isolation around handler
  execution, SDK-side metric counters, client-local circuit-breakers
  guarding the backend connection. FlowFabric surfaces **none** of
  these ergonomically. Consumers wrap `claim_next()` + trait calls
  by hand.

This RFC proposes a narrow, bespoke layer trait that wraps
`Arc<dyn EngineBackend>` at `FlowFabricWorker::connect_with(...)` time.
Non-fenced, purely user-space, strictly additive. Complements — does
not replace — engine primitives.

### §1.1 What consumer pain this closes

1. cairn-fabric hand-rolls `tracing::info_span!` around each `claim_next`
   + `complete`; no composable way to add a span wrapper across all
   backend calls.
2. "Don't fire more than K `complete` calls per second to protect
   downstream" — no engine-side knob fits (flow budgets are per-flow, not
   per-worker-SDK).
3. Handler panics tear the worker loop; consumers reinvent
   `AssertUnwindSafe` wrapping.
4. OTEL bridge (owner-lock 2026-04-22) is engine-scope; consumers wanting
   a second metrics pipeline (StatsD, vendor SDK) have no SDK-side hook.

None require fence semantics. All are client-local.

## §2 Proposed trait shape

### §2.1 Core trait

```rust
/// Wrap an `EngineBackend` with a client-local cross-cutting concern.
///
/// A layer is a pure function from `Arc<dyn EngineBackend>` to a new
/// `Arc<dyn EngineBackend>`. The output delegates to the input for
/// all trait methods, optionally observing or short-circuiting.
///
/// Layers run **client-local only**. They MUST NOT be used to enforce
/// any distributed-correctness invariant (see §4).
pub trait EngineBackendLayer: Send + Sync + 'static {
    fn layer(
        &self,
        inner: Arc<dyn EngineBackend>,
    ) -> Arc<dyn EngineBackend>;
}
```

### §2.2 Blanket impl for closures

```rust
impl<F> EngineBackendLayer for F
where
    F: Fn(Arc<dyn EngineBackend>) -> Arc<dyn EngineBackend>
        + Send + Sync + 'static,
{
    fn layer(&self, inner: Arc<dyn EngineBackend>) -> Arc<dyn EngineBackend> {
        (self)(inner)
    }
}
```

### §2.3 Why not `tower::Service` directly

- `tower::Service<Req>` requires a single request type. `EngineBackend`
  is a trait with 17 heterogeneous methods (RFC-012 round-7 envelope);
  forcing a sum-type request enum duplicates the trait and pessimises
  every call-site.
- `tower::Layer`'s `poll_ready`/`call` split forces pin-boxed futures
  and backpressure reasoning where neither is needed.
- apalis uses tower because `Backend::poll` returns a single
  `Stream<Item = Request<Args>>`. FF's trait is lifecycle ops, not
  poll-shaped.
- Bespoke trait is 12 lines of spec, one method, zero generic soup.
  apalis itself wraps tower in `WorkerBuilderExt` to smooth ergonomics
  (`apalis/src/layers/mod.rs`); we can land the equivalent
  ergonomics without the machinery underneath.

### §2.4 Alternative: per-method hooks enum

Rejected: a `BackendEvent` enum + callback registration grows by variant
(RFC-012 went 13 → 17 methods in two rounds), breaks
non-exhaustive-enum semver on every growth, and loses composition —
trait-object wrapping already gives us dispatch for free.

## §3 Built-in layers (bundled with `ff-sdk`)

Five initial layers. All feature-gated behind `ff-sdk` features
(mirror apalis's `default = ["tracing", "catch-panic", ...]` pattern,
though FF should consider `default = []` given #58 decoupling posture).

### §3.1 `TracingLayer` — feature `layer-tracing`

Wraps each `EngineBackend` method call in a `tracing::info_span!` with
method name + (where available) `handle.execution_id()`. Distinct from
the engine-side `#[instrument]` on Lua dispatch (#170) because this
span lives in the **consumer's** span tree, not the engine's.

Rationale: consumers using `tracing-opentelemetry` with their own
collector want the backend call to appear under the consumer's root
span; engine-side `#[instrument]` emits into the ferriskey OTEL
bridge, a different sink.

### §3.2 `RateLimitLayer` — feature `layer-ratelimit`

Token-bucket rate-cap on configured methods (typical target:
`complete`, `fail`, `append_frame`). Per-worker-process, in-memory.
Enforces "don't exceed K ops/sec from this process"; useful for
consumers that want to protect downstream side-effect sinks. Does
**not** interact with RFC-008 flow budgets — those are per-flow,
engine-enforced, global. This is per-worker, local, best-effort.

Open question §7.1: single bucket for all methods vs per-method
buckets vs a pluggable `fn(&MethodName) -> RateSpec`.

### §3.3 `PanicCatchLayer` — feature `layer-catch-panic`

Wraps each trait call in `FutureExt::catch_unwind`, converting panics
into `EngineError::Bug { .. }`. Rationale: an `EngineBackend` impl
today is assumed panic-free; a hand-rolled consumer layer (e.g. a
`MetricsLayer` with a bad metric name) could panic and would take the
worker loop with it. This gives consumers a safety net for their own
stack of layers. The underlying Valkey backend remains panic-free
regardless.

Note: this covers panics **inside the layer stack**, not inside the
**user handler**. The user-handler panic story belongs with the
higher-op trait (#135 scope), not this RFC.

### §3.4 `MetricsLayer` — feature `layer-metrics`

Emits counters/histograms via a consumer-supplied `MetricsSink` trait
(shape sketched; simple `record_call(&method, duration, outcome)`).
Complements but does not overlap #170's engine-side `#[instrument]`:
engine-side records inside the Lua dispatch; this records at the
**SDK caller** — which sees round-trip time, not engine-time, and is
where consumer SLOs actually live.

Deliberately does not standardise on a specific exporter (Prometheus,
OTEL, StatsD). `MetricsSink` is a 1-method trait, consumer plugs in
anything. Contrast with apalis which ships a hard-wired
`apalis-prometheus` layer — FF stays sink-agnostic per the
OTEL-via-ferriskey owner lock.

### §3.5 `CircuitBreakerLayer` — feature `layer-circuit-breaker`

Opens a circuit on consecutive `EngineError::Transport` errors from
the wrapped backend. While open, fails fast with a synthetic
`Transport` error rather than attempting the wrapped call. Half-open
trial after a cool-down. Rationale: protects the consumer from
hammering a degraded Valkey node; distinct from engine-side retry
(which is about task-level retry semantics on the fenced record).

Open question §7.2: reasonable default thresholds; whether to also
open on `Contention`.

### §3.6 Explicitly NOT bundled

- **`RetryLayer`** (client-local retry). Considered. Rejected for
  v1 because it collides semantically with engine-enforced
  `RetryPolicy` (RFC-005) and would confuse consumers. A consumer
  who wants client-local retry can write one in three lines against
  the trait; we do not want to bless it in the default feature set.
  Reconsider in v2 with a clear name (`TransportRetryLayer`?) and a
  contract that it **only** retries `EngineError::Transport`.
- **`TimeoutLayer`** (client-local timeout). Rejected: lease TTL is
  the truth; a local timeout that fires before the engine releases
  the lease creates a divergent-state bug class. If a consumer
  wants a wall-clock kill switch they wrap with `tokio::time::timeout`
  at the handler level, outside the backend.
- **Sentry / Prometheus exporter layers.** Per OTEL-via-ferriskey
  owner lock, FF does not ship vendor-specific exporters. Consumers
  stack these themselves through `MetricsLayer` + their sink.

## §4 What layers CANNOT do (hard boundary)

A layer sees `Arc<dyn EngineBackend>`. It can observe, short-circuit,
or decorate. It **must not**:

1. **Reshape persistence.** A layer that rewrites KEYS/ARGV or fakes
   Lua return values is a new backend impl, not a layer. Implement
   `EngineBackend` directly.
2. **Claim to enforce the fence triple** (RFC-003). Fence-current /
   active-index / epoch fence are Lua-enforced at the backend. A
   layer cannot synthesise these; attempting to observe and rewrite
   them breaks RFC-003's stale-writer rejection invariant.
3. **Fake lease ownership or waitpoint-resume tokens** (RFC-004).
   HMAC resume tokens are engine-signed; a layer cannot mint or
   validate them. Panic if a consumer tries.
4. **Skip or reorder calls in a way that breaks atomicity.** E.g.
   a layer must not batch `complete` calls. The engine assumes each
   `complete` is a distinct fenced write.
5. **Violate RFC-011 (exec/flow co-location).** A layer must not
   reroute calls across partitions; partition routing is
   `ValkeyBackend`'s concern (or the eventual Postgres backend's).

§6 lists a test-case checklist Stage-1 implementation MUST pass to
guard these boundaries.

## §5 Composition API

### §5.1 Builder-style on `Arc<dyn EngineBackend>`

```rust
use ff_sdk::layer::{EngineBackendLayerExt, TracingLayer, RateLimitLayer};

let backend: Arc<dyn EngineBackend> =
    ValkeyBackend::from_client_and_partitions(client, partitions).await?.into();

let layered = backend
    .layer(TracingLayer::default())
    .layer(RateLimitLayer::per_method("complete", 500))
    .layer(PanicCatchLayer::default());

let worker = FlowFabricWorker::connect_with(config, layered, None).await?;
```

`EngineBackendLayerExt` is a sealed extension trait providing `.layer(L)`
on `Arc<dyn EngineBackend>`. Sealed so we don't bind future extension
points to the current shape.

### §5.2 Ordering semantics

Outer layer runs first (like tower, like apalis — documented at
`apalis/src/layers/mod.rs` "tracing added before retry will see all
retries as a single operation"). Document prominently: "tracing
**outside** rate-limit sees rate-limited-rejections as distinct
events; tracing **inside** sees only successful admissions."

### §5.3 Zero-layer case

`FlowFabricWorker::connect_with(config, backend, None)` continues to
work unchanged. No-op is the default. Every layer is opt-in.

## §6 Interaction with existing engine-enforced policies

| Concern                   | Ownership            | Layer-appropriate? |
|---------------------------|----------------------|--------------------|
| `RetryPolicy` (RFC-005)   | Engine (fenced)      | No — engine only.  |
| Lease TTL (RFC-003)       | Engine (fenced)      | No — engine only.  |
| Flow budget (RFC-008)     | Engine (fenced)      | No — engine only.  |
| Waitpoint HMAC (RFC-004)  | Engine (signed)      | No — engine only.  |
| Local tracing span        | Consumer             | **Yes.**           |
| SDK-side metric counters  | Consumer             | **Yes.**           |
| In-process rate cap       | Consumer             | **Yes.**           |
| Handler panic isolation   | Consumer             | **Yes.**           |
| Transport-level breaker   | Consumer             | **Yes.**           |

Rule of thumb: **if losing the concern causes a distributed-correctness
bug, it's engine-enforced. If losing it causes a consumer-side
observability or ergonomics loss, it's layer-appropriate.**

## §7 Open questions for owner

1. **Default feature set.** apalis default-enables 5 layers;
   `ff-sdk` philosophy so far has been minimal defaults (#58
   decoupling posture). Recommendation: `default = []`, all layers
   opt-in. Owner call.
2. **Circuit-breaker trigger scope.** Transport-only, or also
   `Contention` / `Conflict`? (§3.5).
3. **Rate-limit granularity.** Single bucket / per-method / pluggable
   (§3.2).
4. **`MetricsSink` vs `ff-observability` bridge.** Does the built-in
   `MetricsLayer` go through `ff-observability` (engine OTEL sink) or
   a separate consumer-chosen sink? Recommendation: separate — the
   whole point is to offer a client-local pipeline. Owner to
   confirm consistency with 2026-04-22 OTEL lock.
5. **Layer-ordering enforcement.** Should we refuse `PanicCatchLayer`
   as the innermost layer (unable to catch its own implementation
   bugs) at compile time or runtime? Recommendation: runtime warn
   only, document convention.
6. **Semver of the `EngineBackendLayer` trait itself.** Adding a
   method would break implementers. Recommendation: seal for v1,
   add `#[doc(hidden)] fn __private(&self) {}` to reserve future
   expansion. Owner preference?
7. **Higher-op trait interplay (#135).** When higher-op trait lands,
   do layers wrap the lower `EngineBackend` trait or the higher
   trait? Recommendation: `EngineBackend` only (the lower, stable
   surface); higher-op gets its own extractor story per apalis's
   `FromRequest` pattern (PPPP recommendation #4).

## §8 Alternatives rejected

### §8.1 Adopt `tower::Service` + `tower::Layer` directly

Rejected per §2.3. Machinery cost > ergonomic win for a 17-method
heterogeneous trait. apalis's own `WorkerBuilderExt` demonstrates
you always end up wrapping tower anyway.

### §8.2 No layer surface; consumers wrap `Arc<dyn EngineBackend>`
by hand

Status quo. Works, but (a) every consumer reimplements the same
five concerns, (b) no shared vocabulary for discussions ("add a
rate-limit layer" vs "write a struct that impls 17 methods
delegating to an inner Arc and also does rate-limiting"),
(c) `EngineBackend` grows by RFC amendment (13 → 17 in two rounds);
per-consumer wrappers ossify against old shapes. A sealed layer
trait + the blanket closure impl gives us one stable wrap point.

### §8.3 Per-method callback registration (`on_before_complete`, etc.)

Rejected per §2.4. Non-composable, grows by variant not by
composition, collides with non-exhaustive-enum semver plans.

### §8.4 Make layers themselves fenced

Rejected. By definition fencing lives in Lua + the engine. Layers
that pretend to fence are a footgun. Document in §4 that this is
never allowed.

### §8.5 Replace engine `RetryPolicy` with a `RetryLayer`

Explicitly out of scope. RFC-005's retry is fenced, at-most-once
per attempt number, and survives worker crash. A client-local
layer cannot replicate that. See §3.6 on why we don't even
bundle a `RetryLayer`.

## §9 Semver impact

- **Additive.** New trait, new `ff-sdk::layer` module, new
  feature flags, new ext trait. No change to `EngineBackend` shape
  or any existing SDK API.
- **Consumer ergonomic win.** Existing call sites (zero layers)
  continue to compile unchanged.
- **Target release:** 0.5.0 minor, or 0.4.x point release if owner
  decides the trait is small-enough and implementation risk is low.
  Recommendation: 0.5.0 to co-land with higher-op trait + extractor
  work (one coherent ergonomic refresh).

## §10 Scope estimate

**RFC acceptance:**
- Worker debate round (per `feedback_rfc_team_debate_protocol`):
  high-impact, all-worker review until unanimous ACCEPT. Est. 2–3
  rounds, ~6–10 hours wall-time single-agent with subagent-parallel
  reviews.
- Owner adjudication on §7 open questions: 1 round.

**Implementation (first landing, `ff-sdk`):**
- `EngineBackendLayer` trait + `Arc<dyn EngineBackend>::layer(..)` ext + closure impl: ~2h.
- `TracingLayer`: ~2h (including span-name mapping for all 17 methods).
- `RateLimitLayer`: ~3h (token bucket, governor crate or hand-rolled).
- `PanicCatchLayer`: ~2h (catch_unwind + `EngineError::Bug` mapping).
- `MetricsLayer` + `MetricsSink` trait: ~3h (sink-agnostic, 1-method trait, 17-method dispatch).
- `CircuitBreakerLayer`: ~4h (state machine + half-open trial).
- Tests, including §4 boundary-enforcement cases (layer can't fake
  a fence, can't mint a waitpoint HMAC, can't skip/reorder calls):
  ~6h.
- Docs + examples in `crates/ff-sdk/examples/`: ~3h.

**Total first-landing estimate:** ~25 hours single-agent.
Parallelisable across two workers (layer infra + 2-3 built-ins each).

Stage gates:
1. Accept RFC after debate.
2. Stage 1: trait + ext + `TracingLayer` + `PanicCatchLayer` (smallest
   vertical slice, prove the surface).
3. Stage 2: remaining three built-in layers.
4. Stage 3: cairn-fabric migration example (if requested by consumer
   team; peer-team boundaries respected per
   `feedback_peer_team_boundaries`).

## §11 Citations

- apalis layers: https://github.com/geofmureithi/apalis/blob/main/apalis/src/layers/mod.rs
- apalis features (default layer set): https://github.com/geofmureithi/apalis/blob/main/apalis/Cargo.toml
- tower::Layer: https://docs.rs/tower/latest/tower/trait.Layer.html
- PPPP's deep-dive: `rfcs/drafts/apalis-deep-architecture-dive.md` §"Tower layer composition" (lines 104–129), §Recommendations item 5 (lines 297–301)
- FF `FlowFabricWorker::connect_with`: `crates/ff-sdk/src/worker.rs:574`
- FF `EngineBackend` trait: RFC-012 §3.1.1 (17 methods post-round-7)
- Engine-enforced policies: RFC-003 (fence-triple), RFC-004 (waitpoint/HMAC), RFC-005 (retry), RFC-008 (budget), RFC-011 (exec/flow colocation)
- OTEL-via-ferriskey owner lock: 2026-04-22 release decisions (`project_ff_release_decisions`)
