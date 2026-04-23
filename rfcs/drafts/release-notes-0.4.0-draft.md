# FlowFabric 0.4.0

*Tag date: 2026-04-23 (pending). This draft is synthesized from the
`[Unreleased]` section of `CHANGELOG.md`, `docs/cairn-migration-v0.4.0.md`,
and `rfcs/drafts/0.4.0-release-readiness-audit.md`.*

> **Pending T4 merge (#158):** these notes do **not** yet cover the
> `FlowFabricWorker::client()` seal / stream free-fn relocation that
> closes #87. If T4 lands before the tag, add its bullets to
> **Breaking changes → Infrastructure** and mention the `client()`
> removal in **Highlights**.

## Highlights

0.4.0 carries the full **RFC-012 Round-7 `EngineBackend` trait
surface**. `append_frame`, `create_waitpoint`, and `report_usage`
are real trait methods with backend-parametric return types — no
more `Unavailable` stubs, no SDK-internal direct-FCALL shortcuts.
Alternate backends (Postgres, test doubles) now have a coherent
16-method contract.

Two ferriskey leaks are sealed: **`ferriskey::Error` no longer
appears in `SdkError` / `ServerError`** (replaced by
`ff_core::BackendError`), and **`WorkerConfig` splits its
Valkey-specific fields into a nested `BackendConfig`** carrying
timeouts, retry policy, and connection handles uniformly. Snapshot
decoders now return `EngineError::Validation{Corruption}` on
on-disk corruption, `BackendTimeouts.request` + `BackendRetry` are
actually wired through to `ClientBuilder` (both were silently
dropped before), and additive re-exports +
`ScannerFilter::with_namespace` ergonomics round out cairn's path.

## Breaking changes

### `EngineBackend` trait (RFC-012 Round-7)

- `append_frame` now returns `AppendFrameOutcome { stream_id,
  frame_count }` (was `()`). Type relocates from `ff_sdk::task` into
  `ff_core::backend`; a `pub use` shim keeps the `ff_sdk::task`
  path working through 0.4.x.
- New required method `create_waitpoint(handle, waitpoint_key,
  expires_in) -> PendingWaitpoint`.
- `report_usage` now returns `ReportUsageResult` (was
  `AdmissionDecision`). `AdmissionDecision` is **removed**.
- `UsageDimensions` gains `dedup_key: Option<String>` for trait-level
  idempotency (was reachable only via SDK-private wire).
- Trait method count: 15 → 16.
- `suspend` migration remains deferred to Stage 1d.

### Error surface — ferriskey leak sealed (#88)

- `SdkError::Valkey(ferriskey::Error)` → `SdkError::Backend(BackendError)`.
- `SdkError::ValkeyContext { source, .. }` →
  `SdkError::BackendContext { source: BackendError, .. }`.
- `SdkError::valkey_kind()` → `SdkError::backend_kind()
  -> Option<BackendErrorKind>`.
- `ServerError::Valkey` / `ValkeyContext` → `Backend` / `BackendContext`.
- HTTP `ErrorBody.kind` wire strings change: `"io_error"` →
  `"transport"`; `"cluster_down"` / `"moved"` / `"ask"` /
  `"try_again"` all collapse to `"cluster"`. **Downstream consumers
  matching on these strings must update.**
- `BackendErrorKind::is_retryable` now treats cluster-churn kinds
  (`Moved`, `Ask`, `MasterDown`, `CrossSlot`, …) as retryable,
  matching standard Valkey-cluster client semantics.

Before / after:

```rust
// Before (0.3.x)
match err {
    SdkError::Valkey(e) if e.kind() == ferriskey::ErrorKind::IoError => retry(),
    _ => bail!(err),
}

// After (0.4.0)
use ff_core::BackendErrorKind;
match err.backend_kind() {
    Some(k) if k.is_retryable() => retry(),
    _ => bail!(err),
}
```

### `WorkerConfig` / `BackendConfig` split (RFC-012 Stage 1c T1)

- `WorkerConfig::{host, port, tls, cluster}` moved into the nested
  `backend: BackendConfig` field. `BackendConfig` also carries
  `BackendTimeouts` and `BackendRetry`.
- `WorkerConfig::new` constructor **removed**. Construct via struct
  literal.

```rust
// Before
let cfg = WorkerConfig::new("worker-1", "localhost", 6379);

// After
use ff_core::BackendConfig;
let cfg = WorkerConfig {
    worker_id: "worker-1".into(),
    backend: BackendConfig::valkey("localhost", 6379),
    ..Default::default()
};
```

### `BackendTimeouts` / `BackendRetry` reshape

- `BackendTimeouts.keepalive` **removed** — ferriskey handles TCP
  keepalive unconditionally at OS defaults; the field had no
  observable effect.
- `BackendRetry` fields reshape to match ferriskey's
  `ConnectionRetryStrategy`: `max_attempts` / `base_backoff` →
  `exponent_base` / `factor` / `number_of_retries` /
  `jitter_percent`. The prior shape was semantically mismatched
  with the underlying retry primitive.
- `BackendTimeouts.request` is now actually honored (was silently
  dropped before). Consumers with a custom value will see it
  enforced.

### `Frame` extension (RFC-012 §R7)

- `ff_core::backend::Frame` gains `frame_type: String` and
  `correlation_id: Option<String>` so `append_frame` can forward
  through the trait without wire-parity regression. Still
  `#[non_exhaustive]`; new builder setters `Frame::with_frame_type`
  / `Frame::with_correlation_id`.

### Snapshot decoder errors (Stage 1c T3)

- `describe_execution`, `describe_flow`, `describe_edge`,
  `list_incoming_edges`, `list_outgoing_edges` now surface on-disk
  corruption as `SdkError::Engine(Box<EngineError::Validation {
  kind: ValidationKind::Corruption, .. }>)` instead of
  `SdkError::Config`. Decoders + `FLOW_CORE_KNOWN_FIELDS` relocate
  from `ff-sdk::snapshot` into `ff_core::contracts::decode`.
  `SdkError::Config` is retained for SDK-side input validation.

## Added

- `ff_core::BackendError` enum + `BackendErrorKind` classifier
  (`Transport`, `Protocol`, `Timeout`, `Auth`, `Cluster`,
  `BusyLoading`, `ScriptNotLoaded`, `Other`) with `is_retryable()`
  and stability-fenced `as_stable_str()`.
- `ff_backend_valkey::backend_error` module owning the
  ferriskey → `BackendError` conversion.
- `ff_core::backend::PendingWaitpoint` (carries `waitpoint_id` +
  `hmac_token`, `#[non_exhaustive]`).
- `ff-core` feature-flag scaffold: `default = ["core", "streaming",
  "suspension", "budget"]`. Bodies are empty at T1; tranches 2-5
  will add `#[cfg]` gates on trait methods so alternate backends
  may opt into subsets.
- CI job: `cargo check -p ff-core --no-default-features --features
  core` to catch forgotten `cfg` gates on trait-method additions.
- `ff_test::fixtures::backend_config_from_env` helper (+ re-export
  from `ff_readiness_tests::valkey`).
- `ff_backend_valkey::build_client` is now `pub`;
  `FlowFabricWorker::connect` shares the one mapping from
  `BackendConfig` → `ferriskey::Client`.
- DX re-exports: `ff_core::ScannerFilter`,
  `ff_core::backend::Namespace`,
  `ff_core::backend::BackendConfig`.
- `ScannerFilter::with_namespace` now takes `impl Into<Namespace>`
  (additive; prior `Namespace` call sites still compile).

## Changed

- `FlowFabricWorker::connect` routes through
  `ff_backend_valkey::build_client` instead of its own
  `ClientBuilder` chain.
- `BackendRetry` wired through to
  `ClientBuilder::retry_strategy` when any field is set.
- `ValkeyBackend::describe_execution` / `describe_flow` are now
  implemented (previously returned `Unavailable`). `ff-sdk`'s
  `FlowFabricWorker::describe_*` and `list_*_edges` collapse to
  thin trait forwarders.

## Fixed

- `BackendTimeouts.request` now enforced (#139). Was silently
  dropped via URL-based `Client::connect` before.
- Scenario 4 bench methodology switched to N=5 multi-sample runs
  with mean + stdev (#140); previous single-sample numbers hid
  run-to-run variance. Baselines refreshed at v0.1.0 + HEAD
  (#144). `apalis` comparison now uses `apalis-workflow` DAG
  (#51, #148).

## Migration guide

See [`docs/cairn-migration-v0.4.0.md`](../docs/cairn-migration-v0.4.0.md).
It covers `ScannerFilter` builder, `BackendTimeouts` / `BackendRetry`,
`BackendConfig::valkey`, `connect_with` + `completion_backend()`,
the `BackendError` rename, Round-7 trait deltas, the `WorkerConfig`
split, the `Frame` extension, and the snapshot-decoder
`Validation{Corruption}` reshape.

## Full changelog

See [`CHANGELOG.md`](../CHANGELOG.md) — the `[Unreleased]` section
becomes 0.4.0 at tag time.

Merged PRs in this release: #88 (via #151), #136, #137, #139,
#140, #144, #145, #146, #147, #148, #149, #151-#157. T4 (#158) pending.
