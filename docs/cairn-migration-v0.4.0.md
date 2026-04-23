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
// Valkey impls both EngineBackend and CompletionBackend, so the same
// Arc flows through both positions — one allocation, two trait views.
let valkey = Arc::new(ValkeyBackend::connect(backend_config).await?);
let worker = FlowFabricWorker::connect_with(
    config,
    valkey.clone(),
    Some(valkey),
).await?;

let completion = worker
    .completion_backend()
    .expect("Some(..) was passed to connect_with");

let stream = completion.subscribe_completions_filtered(&filter).await?;
```

**`connect_with` + `completion_backend()` (0.3.4):** the third argument
is an explicit `Option<Arc<dyn CompletionBackend>>`. Pass `Some(arc)`
when the backend supports push-based completion; pass `None` for
backends that do not (future Postgres without LISTEN/NOTIFY, test
mocks). `worker.completion_backend()` returns whatever was passed.

Pre-0.3.4 (`connect_with(config, backend)`) silently returned `None`
from `completion_backend()` on this path because `Arc<dyn
EngineBackend>` cannot be re-upcast to `Arc<dyn CompletionBackend>` in
Rust's trait-object model. 0.3.4 makes the caller decide.

## 6. `SdkError` shape migration (post-RFC-012 Stage 1c T3)

Stage 1c T3 moves the strict-parse decoders for `describe_execution`,
`describe_flow`, and `describe_edge` (plus `list_*_edges`) out of
ff-sdk and into `ff_core::contracts::decode`. Every `EngineBackend`
implementation now shares one error surface:
`EngineError::Validation { kind: ValidationKind::Corruption, detail }`.

### Before (pre-0.4.0 ff-sdk)

```rust
match err {
    ff_sdk::SdkError::Config { field, message, .. } => {
        // field is Option<String>; message is the full context.
        assert_eq!(field.as_deref(), Some("public_state"));
    }
    other => panic!("expected Config, got {other:?}"),
}
```

### After (0.4.0+)

```rust
match err {
    ff_sdk::SdkError::Engine(boxed) => match boxed.as_ref() {
        ff_core::engine_error::EngineError::Validation {
            kind: ff_core::engine_error::ValidationKind::Corruption,
            detail,
        } => {
            // detail is a "<context>: <field>: <message>" string;
            // substring-match the field name.
            assert!(detail.contains("public_state"));
        }
        other => panic!("expected Validation::Corruption, got {other:?}"),
    },
    other => panic!("expected SdkError::Engine, got {other:?}"),
}
```

### Scope

Only on-disk-corruption paths change shape. This includes:

- `describe_execution` — exec_core / tags hash parse failures.
- `describe_flow` — flow_core hash parse failures, unknown-field sweep.
- `describe_edge`, `list_incoming_edges`, `list_outgoing_edges` — edge
  hash parse failures, adjacency-set endpoint drift.

`SdkError::Config` still exists and is still returned for SDK-side
input validation (e.g. `WorkerConfig` with zero lanes). Only the
subset of `SdkError::Config` emitted by the snapshot decoders
collapses into `SdkError::Engine(EngineError::Validation { Corruption, .. })`.

### Detail format

`detail` is a human-readable string with the shape
`"<context>: <field>: <message>"` (field omitted for whole-object
errors). The `field` is embedded — not a typed slot — so consumers
that want field-level routing should substring-match. FF's own tests
use `assert!(detail.contains("<field>"))`; this is the recommended
pattern.

### Classification

`EngineError::Validation` classifies as
`ErrorClass::Terminal` (see `EngineError::class()`), so
`SdkError::is_retryable()` returns `false` for these — same retry
posture as the pre-migration `SdkError::Config`.

## 7. Gotchas (cross-cutting)

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

## 8. `Frame` gained `frame_type` + `correlation_id` (#147)

```rust
use ff_core::backend::{Frame, FrameKind};

// Preferred: constructor + builder chainers.
let frame = Frame::new(bytes, FrameKind::Output)
    .with_frame_type("tool_call")
    .with_correlation_id(request_id);

// Struct-literal callers MUST use `..Default::default()` or a helper
// — `Frame` is `#[non_exhaustive]` and gained two fields in 0.4.0.
let frame = Frame {
    bytes,
    kind: FrameKind::Output,
    seq: None,
    frame_type: String::from("tool_call"),
    correlation_id: Some(request_id),
    // If this line is omitted, the literal fails with E0063 on any
    // future field addition; keep it even when naming every current
    // field explicitly.
    ..Default::default()
};
```

**What changed.** `ff_core::backend::Frame` (`crates/ff-core/src/backend.rs:207`) added:
- `frame_type: String` — free-form classifier forwarded to the Lua-side
  `frame_type` ARGV. Empty string means "defer to `FrameKind`", and the
  backend substitutes the enum-variant encoding.
- `correlation_id: Option<String>` — wire `correlation_id` ARGV; `None`
  encodes as the empty string.

**Gotcha.** `FrameKind` is *not* gone. The pre-0.4.0 `kind` field stays
as the typed classification; `frame_type` is additive for SDK
forwarders that carry a string-valued type tag. If you only have a
`FrameKind`, leave `frame_type` as `String::new()` and the backend
falls back to `frame_kind_to_str` (see `ff-backend-valkey`).

**Rationale.** SDK forwarders on the proxy-style path need to preserve
upstream classifiers and correlation identifiers verbatim; boxing them
through `FrameKind` would have forced a variant explosion on every new
consumer shape. Source: PR #147; struct definition
`crates/ff-core/src/backend.rs:205-220`; constructors
`:230-260`.

## 9. `EngineBackend` trait deltas (#145, Round-7)

Three breaking changes land together on the trait surface. Impls
written against 0.3.x will not compile.

### 9.1 `append_frame` return widened to `AppendFrameOutcome`

```rust
// Before (0.3.x):
async fn append_frame(&self, ..., frame: Frame) -> Result<(), EngineError>;

// After (0.4.0):
async fn append_frame(&self, ..., frame: Frame)
    -> Result<AppendFrameOutcome, EngineError>;
```

`AppendFrameOutcome` (`crates/ff-core/src/backend.rs:638`) carries:

```rust
pub struct AppendFrameOutcome {
    pub stream_id: String,  // Valkey Stream entry id, e.g. "1234567890-0"
    pub frame_count: u64,   // total frames in the stream post-append
}
```

Not `#[non_exhaustive]` — construction is backend-internal
(parser in `ff-backend-valkey`); consumers destructure or read fields.

**Gotcha.** Callers that discarded the unit return (`let _ = backend.append_frame(...).await?;`)
keep compiling but now discard the outcome; touch those sites if you
want the stream id or frame count for logging.

### 9.2 New `create_waitpoint` trait method

```rust
async fn create_waitpoint(
    &self,
    handle: &Handle,
    waitpoint_key: &str,
    expires_in: Duration,
) -> Result<PendingWaitpoint, EngineError>;
```

Every `EngineBackend` impl must provide this method. `PendingWaitpoint`
lives at `crates/ff-core/src/backend.rs:322`. Expiry is a first-class
terminal error on the wire (`PendingWaitpointExpired` in
`ff-script/src/error.rs`).

Source: `crates/ff-core/src/engine_backend.rs:161`.

### 9.3 `report_usage` returns `ReportUsageResult`; `AdmissionDecision` deleted

```rust
// Before (0.3.x):
async fn report_usage(...) -> Result<AdmissionDecision, EngineError>;

// After (0.4.0):
async fn report_usage(...) -> Result<ReportUsageResult, EngineError>;
```

`AdmissionDecision` is removed from the public surface. The
replacement (`crates/ff-core/src/contracts.rs:1402`) is a richer
variant set:

```rust
#[non_exhaustive]
pub enum ReportUsageResult {
    Ok,
    SoftBreach { dimension: String, current_usage: u64, soft_limit: u64 },
    HardBreach { dimension: String, current_usage: u64, hard_limit: u64 },
    AlreadyApplied,  // dedup key matched; no double-count
}
```

**Migration mapping.** Previous admit/deny callers collapse roughly as:

| Pre-0.4.0                       | Post-0.4.0                         |
| ------------------------------- | ---------------------------------- |
| `AdmissionDecision::Admit`      | `ReportUsageResult::Ok`            |
| `AdmissionDecision::Deny { .. }`| `ReportUsageResult::HardBreach {..}` |
| (none — silently double-counted)| `ReportUsageResult::AlreadyApplied`|
| (none)                          | `ReportUsageResult::SoftBreach {..}`|

`SoftBreach` is advisory: increments **are** applied. `HardBreach`
rejects the report and does **not** apply increments. `AlreadyApplied`
is the dedup success path (see §11).

**Gotcha.** `ReportUsageResult` is `#[non_exhaustive]` — match arms
must include a wildcard, and exhaustive matches against the 0.3.x
two-variant enum become non-exhaustive warnings in 0.4.0.

Source: PR #145; `crates/ff-core/src/engine_backend.rs:221-230`.

## 10. `BackendError` seal: `SdkError::Valkey*` → `SdkError::Backend*` (#151, HIGH IMPACT)

The public error surface no longer leaks `ferriskey::Error`. Every
consumer that matched on `SdkError::Valkey` / `ServerError::Valkey*`
or called `valkey_kind()` MUST update.

### 10.1 Rust error variants

```rust
// Before (0.3.x):
match err {
    SdkError::Valkey(fk_err) => { /* match on ferriskey::ErrorKind */ }
    SdkError::ValkeyContext { source, context } => { ... }
    _ => { ... }
}

// After (0.4.0):
use ff_sdk::{BackendError, BackendErrorKind};

match err {
    SdkError::Backend(be) => {
        match be.kind() {
            BackendErrorKind::Transport    => { /* retry */ }
            BackendErrorKind::Timeout      => { /* retry */ }
            BackendErrorKind::Cluster      => { /* retry after settle */ }
            BackendErrorKind::BusyLoading  => { /* retry */ }
            BackendErrorKind::Auth         => { /* surface, do NOT retry */ }
            BackendErrorKind::Protocol     => { /* surface */ }
            BackendErrorKind::ScriptNotLoaded => { /* re-load */ }
            BackendErrorKind::Other        => { /* log + surface */ }
        }
    }
    SdkError::BackendContext { source, context } => { ... }
    _ => { ... }
}
```

`ServerError::Valkey` / `ServerError::ValkeyContext` renamed to
`ServerError::Backend` / `ServerError::BackendContext` in the same
shape (source `crates/ff-server/src/server.rs:224,302,318`).

### 10.2 Classifier rename: `valkey_kind()` → `backend_kind()`

```rust
// Before: err.valkey_kind() -> Option<ferriskey::ErrorKind>
// After:  err.backend_kind() -> Option<BackendErrorKind>
```

`BackendErrorKind` is `#[non_exhaustive]` — include a wildcard arm.
`::is_retryable()` and `::as_stable_str()` are stable on this type
(source `crates/ff-core/src/engine_error.rs:413-460`).

### 10.3 HTTP `ErrorBody.kind` wire strings changed

This is the **most subtle** breaking change in #151, because it
silently changes HTTP bodies without any Rust-compile signal.

Pre-0.4.0, ff-server rendered `ErrorBody.kind` from the ferriskey
native-kind debug string (e.g. `"IoError"`, `"ClusterDown"`,
`"MasterDown"`, `"ConnectionNotFoundForRoute"`). Post-0.4.0 (#151),
`ff-server` emits the stable classification from
`BackendErrorKind::as_stable_str()`
(`crates/ff-server/src/api.rs:251-282`):

| `BackendErrorKind` variant | Wire string           |
| -------------------------- | --------------------- |
| `Transport`                | `"transport"`         |
| `Protocol`                 | `"protocol"`          |
| `Timeout`                  | `"timeout"`           |
| `Auth`                     | `"auth"`              |
| `Cluster`                  | `"cluster"`           |
| `BusyLoading`              | `"busy_loading"`      |
| `ScriptNotLoaded`          | `"script_not_loaded"` |
| `Other`                    | `"other"`             |

**Consumer action (cairn-fabric HTTP client).** Any `match` on
`ErrorBody.kind` string values must switch from the ferriskey debug
spellings to the stable-kebab set above. A bridge layer that
previously held `fn retryable(k: &str) -> bool` keyed on ferriskey
spellings needs a new table keyed on `BackendErrorKind::as_stable_str()`
outputs.

**Gotcha.** The variant set is curated — several ferriskey kinds
collapse to `Other`. Callers that relied on fine-grained ferriskey
kinds to drive behavior (e.g. distinguishing `MasterDown` from
`ClusterDown`) must accept the coarser bucket. ff-core's
`BackendErrorKind::is_retryable` owns the canonical retry decision
(`crates/ff-core/src/engine_error.rs:573-587`); prefer calling that
over rolling a parallel table.

**Rationale.** Sealing ferriskey from the public surface (#88) keeps
ff-sdk consumers out of ferriskey's SemVer — a backend swap doesn't
cascade a major bump through downstream crates. Source: PR #151;
`crates/ff-core/src/engine_error.rs:347-460`, `crates/ff-sdk/src/lib.rs:93-119`.

## 11. `UsageDimensions` gained `dedup_key: Option<String>` (Round-7)

```rust
use ff_core::backend::UsageDimensions;

let dims = UsageDimensions {
    input_tokens: 1200,
    output_tokens: 340,
    wall_ms: Some(850),
    dedup_key: Some(request_id.clone()),
    ..Default::default()
};
```

**What changed.** `UsageDimensions` (`crates/ff-core/src/backend.rs:417`)
gained `dedup_key: Option<String>`. The field is additive; pre-0.4.0
construction sites compile after adding `..Default::default()` (the
type is `#[non_exhaustive]` and already `Default`).

**Semantics.** `Some(key)` threads through to
`ff_report_usage_and_check`'s trailing ARGV. A repeat call with the
same key returns `ReportUsageResult::AlreadyApplied` instead of
double-counting. `None` / empty string disables dedup and restores
pre-0.4.0 semantics.

**Gotcha.** This is **not** the legacy `UsageDimensions::custom`
magic-entry dedup id called out in §7. `dedup_key` is a first-class
field; the `custom`-map variant remains for the
`usage_dedup_key(hash_tag, dedup_id)` callsite documented in §7.
Reports that set *both* produce undefined-at-doc-write-time
precedence — pick one path per report.

Source: RFC-012 §R7.4 (Round-7 impl); field definition
`crates/ff-core/src/backend.rs:434-441`.

## 12. `WorkerConfig` hoists backend fields into `BackendConfig` (T1, PR #146)

> **Status note.** PR #146 is **in-flight at doc-write time
> (2026-04-22).** The shape below reflects the accepted T1 plan; the
> exact constructor signature may shift slightly in the final merged
> commit. Cross-check `crates/ff-sdk/src/config.rs` on `origin/main`
> before relying on this section verbatim.

```rust
// Before (0.3.x, still on main as of doc-write):
let config = WorkerConfig::new(
    "127.0.0.1", 6379,
    "gpu-worker", "instance-1", "tenant-a", "lane-fast",
);

// After (post-T1, 0.4.0 target shape):
use ff_core::backend::BackendConfig;
use ff_sdk::WorkerConfig;

let config = WorkerConfig {
    backend: BackendConfig::valkey("127.0.0.1", 6379),
    worker_id: WorkerId::new("gpu-worker"),
    worker_instance_id: WorkerInstanceId::new("instance-1"),
    namespace: Namespace::new("tenant-a"),
    lanes: vec![LaneId::new("lane-fast")],
    capabilities: Vec::new(),
    lease_ttl_ms: 30_000,
    claim_poll_interval_ms: 1_000,
    max_concurrent_tasks: 1,
    // ..Default::default() where T1 lands a Default impl; otherwise
    // every field is named explicitly.
};
```

**What changed.** `WorkerConfig` loses its inline `host: String` /
`port: u16` / `tls: bool` / `cluster: bool` fields. Those move under
`pub backend: BackendConfig`, which owns the full backend surface
(host, port, TLS, cluster, timeouts, retry) — matching the
`BackendConfig::valkey(host, port)` entry point already documented
in §4.

The `WorkerConfig::new(host, port, ...)` constructor is **removed**.
Consumers build a `BackendConfig` first (via `::valkey(host, port)`
plus field-mut for TLS / cluster / timeouts / retry per §2–§4), then
embed it via struct-literal — or via whatever builder pattern T1
lands.

**Gotcha.** TLS and cluster flags previously on `WorkerConfig`
directly are now reached through `config.backend.tls` /
`config.backend.cluster` (or the `BackendConfig` builder/field-mut
path — ZZ's T1 plan decides the shape). Callers grepping for `.tls`
on `WorkerConfig` will miss the relocated field.

**Rationale.** Pulling backend tunables into `BackendConfig` lets a
Postgres-backed `WorkerConfig` compose the same outer shape against
a `BackendConfig::postgres(...)` constructor, without polluting
`WorkerConfig` with backend-specific fields. This is the structural
precondition for the RFC-#58 decoupling work.

Source: PR #146 (in-flight); current pre-T1 shape
`crates/ff-sdk/src/config.rs:1-61`.

## 13. Stream access reshape + `client()` sealed (T4, PR #158)

Stage 1c tranche-4 finishes the ferriskey-seal on the worker-facing
surface. Two shifts land together.

### 13.1 `FlowFabricWorker::client()` → `pub(crate)`

```rust
// Before (0.3.x): leaked ferriskey::Client.
let client = worker.client();

// After (0.4.0): accessor is crate-private. Replacements:
//   - Typed ops: go through SDK methods on `FlowFabricWorker` / `ClaimedTask`.
//   - Stream reads: `ClaimedTask` methods (§13.2).
//   - Tests: `TestCluster::backend()` → `Arc<dyn EngineBackend>`.
```

The #158 grep sweep found zero non-example consumers; ferriskey no
longer surfaces through any worker-facing accessor.

### 13.2 `read_stream` / `tail_stream` — canonical path is `ClaimedTask`

```rust
// Before (0.3.x): free fns over `&ferriskey::Client` + `&PartitionConfig`.
use ff_sdk::task::{read_stream, tail_stream};
let frames = read_stream(worker.client(), &config, &handle, opts).await?;

// After (0.4.0): method on the task you already hold.
let frames = task.read_stream(opts).await?;
let stream = task.tail_stream(opts).await?;

// Free fns still exist with a reshaped signature for non-task callers
// (tests, consumers holding a bare backend). They now take
// `&dyn EngineBackend` instead of `&Client` + `&PartitionConfig`:
use ff_sdk::task::read_stream;
let frames = read_stream(&*backend, &handle, opts).await?;
```

Validation (`count_limit` bounds, tail-cursor shape) stays at the SDK
edge; `SdkError::Config` semantics are preserved across both paths.

Source: PR #158; `ff-core::EngineBackend::{read_stream,tail_stream}`
(gated on the default-on `streaming` feature),
`ff-sdk::ClaimedTask::{read_stream,tail_stream}`.

## 14. DX polish: re-exports + `Into<Namespace>` (PR #157)

Four additive re-exports / signature widenings — no break, purely
cuts `use`-statement ceremony for consumers:

```rust
// New short paths (canonical paths still work):
use ff_core::ScannerFilter;          // was ff_core::backend::ScannerFilter
use ff_core::backend::Namespace;     // was ff_core::types::Namespace
use ff_backend_valkey::BackendConfig; // was ff_core::backend::BackendConfig

// `ScannerFilter::with_namespace` now takes `impl Into<Namespace>`:
let filter = ScannerFilter::new().with_namespace("tenant-a"); // &str works
let filter = ScannerFilter::new().with_namespace(ns);          // Namespace works
```

`Namespace` is a `string_id!` newtype, so `From<&str>` / `From<String>`
are infallible — no `TryInto` plumbing required. Existing
`Namespace::new(s)` call sites still compile via the identity `From<Namespace>`.

Source: PR #157.

## References

- RFC-012 Stage 1a: type introduction (landed).
- `rfcs/drafts/backend-timeouts-retry-audit.md` — audit behind #136/#137/#139.
- `rfcs/drafts/0.4.0-release-readiness-audit.md` — Worker III audit
  identifying the §7–§11 gaps filled here.
- PR #145 — trait deltas (`append_frame` outcome, `create_waitpoint`,
  `report_usage` → `ReportUsageResult`, `AdmissionDecision` deleted).
- PR #146 (in-flight) — T1 `WorkerConfig` / `BackendConfig` nesting.
- PR #147 — `Frame` gains `frame_type` + `correlation_id`.
- PR #151 — `BackendError` seal; `SdkError::Valkey*` → `SdkError::Backend*`;
  HTTP wire-kind strings change.
- PR #157 — DX polish: `ff_core::ScannerFilter` / `ff_core::backend::Namespace`
  / `ff_backend_valkey::BackendConfig` re-exports +
  `ScannerFilter::with_namespace: impl Into<Namespace>`.
- PR #158 — Stage 1c T4: `FlowFabricWorker::client()` sealed to
  `pub(crate)`; `read_stream` / `tail_stream` move onto `ClaimedTask`
  (free fns reshape to `&dyn EngineBackend`).
- `docs/rfc011-migration-for-consumers.md` — prior consumer migration.
