# cairn (and other consumers) migration guide for FlowFabric v0.4.0

Reference for constructing the reshaped `ff-core` public types that ship in
v0.4.0 (RFC-012 Stage 1a landed types + follow-up reshapes #136/#137/#139).

Motivated by Worker RR's 0.3.3 smoke finding: `BackendTimeouts` and
`BackendRetry` now carry `#[non_exhaustive]`, so external struct literals
fail to compile (E0639). This doc shows the patterns that do compile across
crate boundaries.

**Scope.** Construction shapes only. Stage 1c (`WorkerConfig` forwarding
of backend tunables) is a separate doc — to be written once the Stage 1c
plan lands.

All file:line citations are against `origin/main` at branch point.

## 1. `ScannerFilter`

Available since 0.3.x. Documented here for completeness because 0.4.0
consumers will pair it with the reshaped types below.

```rust
use ff_core::backend::{ScannerFilter, Namespace};

let filter = ScannerFilter::new()
    .with_namespace(Namespace::new("tenant-a"))
    .with_instance_tag("cairn.instance_id", instance_id);
```

`ScannerFilter` is `#[non_exhaustive]`; use `::new()` + `with_*` chainers.
Source: `crates/ff-core/src/backend.rs:723` (struct), `:742` (`new`),
`:754` (`with_namespace`), `:760` (`with_instance_tag`).

## 2. `BackendTimeouts` (post-#136 keepalive drop, post-#139 request wiring)

```rust
use ff_core::backend::BackendTimeouts;
use std::time::Duration;

let timeouts = BackendTimeouts {
    request: Some(Duration::from_secs(5)),
    ..Default::default()
};
```

`BackendTimeouts` carries `#[non_exhaustive]`
(`crates/ff-core/src/backend.rs:569`). From outside `ff-core`, a bare
struct literal `BackendTimeouts { request: Some(..) }` fails with **E0639**.
You **must** use functional-update syntax (`..Default::default()`), or
construct a `default()` binding and assign the field mutably.

Single field today: `request: Option<Duration>` (`None` ⇒ backend default).

## 3. `BackendRetry` (post-#137 reshape to ferriskey's `ConnectionRetryStrategy`)

```rust
use ff_core::backend::BackendRetry;

let retry = BackendRetry {
    exponent_base: Some(2),
    factor: Some(100),
    number_of_retries: Some(3),
    jitter_percent: Some(20),
    ..Default::default()
};

// Or: all None → ferriskey builder defaults.
let retry = BackendRetry::default();
```

Fields match ferriskey's `ConnectionRetryStrategy` 1:1 (see the comment
at `crates/ff-core/src/backend.rs:577-585`). Each field is `Option<u32>`:
`None` ⇒ ferriskey's builder default, `Some(v)` ⇒ pass-through.

Source: `crates/ff-core/src/backend.rs:588-601`.

## 4. `BackendConfig` composing it all

```rust
use ff_core::backend::BackendConfig;

let mut config = BackendConfig::valkey("127.0.0.1", 6379);
config.timeouts = timeouts;
config.retry = retry;
```

**Today's shape (main, 2026-04-22):** `impl BackendConfig` only ships the
`valkey(host, port)` constructor (`crates/ff-core/src/backend.rs:657`).
There is no `with_timeouts` / `with_retry` / `with_namespace` builder
yet. Field-level assignment on a `mut` binding is how you customize.
`BackendConfig` itself is `#[non_exhaustive]` so cross-crate struct
literals are unavailable — start from `::valkey()` and mutate.

Namespace does **not** live on `BackendConfig`; it lives on
`WorkerConfig` (`crates/ff-sdk/src/config.rs:18`) and on `ScannerFilter`.

## 5. `FlowFabricWorker` + completion subscription

```rust
let worker = FlowFabricWorker::connect_with(config, backend).await?;

let completion = worker
    .completion_backend()
    .expect("valkey-default feature provides CompletionBackend impl");

let stream = completion.subscribe_completions_filtered(&filter).await?;
```

**Important caveat on `connect_with` + `completion_backend()`:**
`connect_with(config, Arc<dyn EngineBackend>)` clears the default
`CompletionBackend` handle because a caller-supplied `Arc<dyn
EngineBackend>` cannot be re-upcast. Consequently
`worker.completion_backend()` returns `None` on that path
(`crates/ff-sdk/src/worker.rs:522-569`).

If you need completion subscription, use `FlowFabricWorker::connect`
(the default path) under the `valkey-default` feature instead, and call
`completion_backend()` on the result — that returns `Some(...)`.

## 6. Gotchas

- **`#[non_exhaustive]` forces `..Default::default()`** on
  `BackendTimeouts`, `BackendRetry`, and most other `ff-core::backend`
  structs. Bare struct literals from outside `ff-core` fail with E0639.
  `BackendConfig` itself has no `Default` impl — start from
  `BackendConfig::valkey(host, port)` and mutate fields.
- **`ff-sdk --no-default-features` compiles but does not drop
  `ferriskey` from the full build graph today.** `ff-script` carries
  an unconditional `ferriskey` dep; the agnosticism claim is scoped to
  `ff-sdk`'s own public surface, not the workspace build graph. See
  `crates/ff-sdk/Cargo.toml:16-28` for the authoritative comment.
- **`BackendRetry` all-`None` ≠ "no retries".** It means "fall back to
  ferriskey's `ConnectionRetryStrategy::default()`." To actually disable
  retries, set `number_of_retries: Some(0)` explicitly.
- **`completion_backend()` returns `None` after `connect_with`.**
  See §5. Use `connect` if you need the completion stream.
- **`UsageDimensions::custom` carries the dedup key inline.** The
  dedup id is embedded as a magic entry inside the `BTreeMap<String,
  u64> custom` map (`crates/ff-core/src/backend.rs:351`,
  `crates/ff-core/src/engine_backend.rs:189-196`), not a dedicated
  field. The canonical key layout is owned by
  `usage_dedup_key(hash_tag, dedup_id)` at
  `crates/ff-core/src/keys.rs:644`. Use that helper; do not hardcode
  `format!("ff:usagededup:{hash_tag}:{dedup_id}")` parallel to it.

## References

- RFC-012 Stage 1a: type introduction (landed).
- `rfcs/drafts/backend-timeouts-retry-audit.md` — audit behind #136/#137/#139.
- `docs/rfc011-migration-for-consumers.md` — prior consumer migration.
