# Changelog

All notable changes to FlowFabric are documented here. Format loosely
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Breaking changes

- **Seal `ferriskey::Error` leak via `BackendError` wrapper (#88):**
  Public SDK + server error surfaces no longer name `ferriskey::Error` /
  `ferriskey::ErrorKind` directly. A new `ff_core::BackendError` +
  `ff_core::BackendErrorKind` taxonomy (non-exhaustive, backend-agnostic)
  replaces the raw ferriskey carriers so consumers can classify transport
  faults without a ferriskey dependency.
  - `ff-core`: new `BackendError` enum (`Valkey` variant today;
    additional backends added additively) + `BackendErrorKind`
    classifier (`Transport`, `Protocol`, `Timeout`, `Auth`,
    `Cluster`, `BusyLoading`, `ScriptNotLoaded`, `Other`) with
    `is_retryable()` + stability-fenced `as_stable_str()`.
  - `ff-backend-valkey`: new `backend_error` module owning the
    ferriskey → `BackendError` conversion
    (`backend_error_from_ferriskey`, `classify_ferriskey_kind`,
    `BackendErrorWrapper`).
  - `ff-sdk`: `SdkError::Valkey(ferriskey::Error)` →
    `SdkError::Backend(BackendError)`; `SdkError::ValkeyContext
    { source: ferriskey::Error, .. }` → `SdkError::BackendContext
    { source: BackendError, .. }`. `SdkError::valkey_kind()
    -> Option<ferriskey::ErrorKind>` renamed to
    `SdkError::backend_kind() -> Option<BackendErrorKind>`. New
    `From<ferriskey::Error> for SdkError` preserves `?`-propagation
    at internal call sites.
  - `ff-server`: `ServerError::Valkey` / `ValkeyContext` similarly
    reshaped to `Backend(BackendError)` / `BackendContext`.
    `ServerError::valkey_kind` renamed to `backend_kind`. HTTP
    `ErrorBody.kind` now sources the wire string from
    `BackendErrorKind::as_stable_str()` instead of ferriskey's
    `kind_to_stable_str`; values change (`"io_error"` →
    `"transport"`, `"cluster_down"` / `"moved"` / `"ask"` /
    `"try_again"` → `"cluster"`, etc.) — downstream consumers
    (cairn-fabric) must update any `kind`-string matching.
  - Behavioral refinement: `BackendErrorKind::is_retryable` treats
    cluster-churn kinds (`Moved`, `Ask`, `MasterDown`, `CrossSlot`,
    `ConnectionNotFoundForRoute`, etc.) as retryable where the
    previous ff-script table treated them as terminal.
    `FatalReceiveError` + `ProtocolDesync` likewise bucket into
    retryable `Transport`. This aligns with standard Valkey-cluster
    client semantics where these are redirects/transients the caller
    should retry.

- **`ff_core::backend::Frame` extended (RFC-012 §R7, PR #147):**
  Added `frame_type: String` + `correlation_id: Option<String>` so
  `ClaimedTask::append_frame` can forward through the `EngineBackend`
  trait without wire-parity regression. Closes the Round-7
  append_frame SDK-forwarder gap flagged in PR #145. `Frame` remains
  `#[non_exhaustive]`; new `Frame::with_frame_type` /
  `Frame::with_correlation_id` builder setters. The Valkey impl
  uses `frame.frame_type` as the free-form `frame_type` ARGV when
  non-empty, falling back to the `FrameKind` encoding when callers
  populate only the typed `kind`.

- **`EngineBackend` trait (RFC-012 §R7, #117):**
  - `append_frame` now returns `AppendFrameOutcome { stream_id,
    frame_count }` (was `()`). The type moves from `ff_sdk::task`
    to `ff_core::backend`; a `pub use` shim preserves the
    `ff_sdk::task::AppendFrameOutcome` path through 0.4.x. Derives
    widen from `Clone, Debug` to `Clone, Debug, PartialEq, Eq`
    matching the `FailOutcome` precedent.
  - New trait method `create_waitpoint(handle, waitpoint_key: &str,
    expires_in: Duration) -> PendingWaitpoint`. `PendingWaitpoint`
    is new in `ff_core::backend` (`#[non_exhaustive]`; carries
    `waitpoint_id` + `hmac_token`).
  - `report_usage` now returns `ReportUsageResult` (was
    `AdmissionDecision`). `ReportUsageResult` gains
    `#[non_exhaustive]` in the same bundle so the replace-over-
    widen is atomic.
  - `AdmissionDecision` removed.
  - `UsageDimensions` gains `dedup_key: Option<String>`
    (idempotency on trait-level `report_usage`; previously only
    reachable via the SDK-private wire).
  - Trait method count 15 → 16; `async fn` compile count 16 → 17.
    `suspend` migration remains deferred to Stage 1d per §R7.1 /
    §R7.6.1 (entangled with input-shape rework).
  - External `EngineBackend` impls: none in tree.
    `ff-backend-valkey`'s `append_frame`, `create_waitpoint`,
    `report_usage` bodies move from `Unavailable` stubs to real
    Lua-backed impls (byte-for-byte wire parity with the SDK's
    pre-migration bodies).
  - `ff-sdk::ClaimedTask::{report_usage, create_pending_waitpoint}`
    become thin trait forwarders. Public SDK signatures unchanged.
    `ClaimedTask::append_frame` stays direct-FCALL pending a
    `Frame`-shape extension (to preserve the free-form `frame_type`
    strings that 5 in-tree callers rely on); closes in Stage 1d
    alongside `suspend` input-shape work.

- **`WorkerConfig` split (RFC-012 Stage 1c tranche 1).** The four
  Valkey-specific fields (`host`, `port`, `tls`, `cluster`) moved into
  the nested `backend: BackendConfig` field, which also carries
  `BackendTimeouts` + `BackendRetry` policy. Worker-policy fields
  (`worker_id`, `worker_instance_id`, `namespace`, `lanes`,
  `capabilities`, `lease_ttl_ms`, `claim_poll_interval_ms`,
  `max_concurrent_tasks`) stay on `WorkerConfig`. Breaking struct
  reshape; pre-1.0 posture accepts.

  Migration: construct `WorkerConfig { backend:
  BackendConfig::valkey(host, port), worker_id: ..., ... }`. For TLS
  or cluster mode, build a `ValkeyConnection` with the flags set and
  overwrite `BackendConfig.connection`. For tuned request timeouts /
  retry, populate `BackendConfig.timeouts` / `BackendConfig.retry`.

- **`WorkerConfig::new` constructor removed** (RFC-012 Stage 1c
  tranche 1). Pre-1.0 clean break — no deprecated shim. Construct via
  struct literal.

- Reshaped `BackendRetry` to match ferriskey's `ConnectionRetryStrategy`
  (fields: `exponent_base`, `factor`, `number_of_retries`,
  `jitter_percent`). Previous `max_attempts`/`base_backoff` were
  semantically mismatched with the underlying retry primitive; the new
  shape is a direct pass-through. Breaking field reshape; pre-1.0
  posture accepts.

- **`BackendTimeouts.keepalive` field removed.** ferriskey handles TCP
  keepalive unconditionally with OS-default settings
  (`ferriskey/src/connection/tokio.rs:33-36`); the field was never wired
  and had no observable behavior. Consumers can re-add custom-interval
  wiring if needed via a future additive field. Breaking public-field
  removal accepted under pre-1.0 posture.

### Added

- **`ff-core` feature-flag scaffold (RFC-012 Stage 1c tranche 1).**
  `[features]` section declared with
  `default = ["core", "streaming", "suspension", "budget"]`. Feature
  bodies are intentionally empty at tranche 1; tranches 2-5 add
  `#[cfg(feature = …)]` gates on `EngineBackend` trait methods so
  backend crates may opt into a strict subset (e.g. a hypothetical
  Postgres backend without `suspension`). Consumer API through
  `ff-sdk` is unchanged — `ff-sdk` pulls default features.
- **CI: `cargo check -p ff-core --no-default-features --features
  core`** job in `matrix.yml` so a forgotten `cfg` gate on a future
  trait-method add fails CI rather than silently widening the
  mandatory-implementation surface.
- **`ff_test::fixtures::backend_config_from_env`** helper (and a
  re-export from `ff_readiness_tests::valkey`) that reads the same
  `FF_HOST` / `FF_PORT` / `FF_TLS` / `FF_CLUSTER` env vars as
  `build_client_from_env` and returns a `BackendConfig`. Used by
  integration tests that construct `WorkerConfig` through the new
  nested-backend shape.
- **`ff_backend_valkey::build_client`** is now `pub`. `FlowFabricWorker::connect`
  reuses it so the `BackendConfig` → `ferriskey::Client` mapping lives
  in exactly one place.

### Changed

- **`FlowFabricWorker::connect` now routes through
  `ff_backend_valkey::build_client`** instead of carrying its own
  `ClientBuilder` chain. Host/port, TLS, cluster mode, request
  timeout, and retry strategy now all wire through one code path
  shared with `ValkeyBackend::connect`.
- Wired `BackendRetry` to ferriskey `ClientBuilder::retry_strategy` in
  `ValkeyBackend::connect`. All 4 fields honored when set; when
  all-`None`, ferriskey's builder default is used (no call to
  `.retry_strategy`).

## [0.3.4] - 2026-04-22

### Fixed

- **`FlowFabricWorker::connect_with` now takes `completion: Option<Arc<dyn CompletionBackend>>` as its third argument.** Breaking signature change.
  - `Some(arc)` = caller-supplied completion backend (e.g. `Arc::clone(&valkey)` since `ValkeyBackend` impls both `EngineBackend` and `CompletionBackend`).
  - `None` = this backend does not support push-based completion (future Postgres backend without LISTEN/NOTIFY; test mocks).
  - Fixes 0.3.3 bug where `connect_with` silently nulled `completion_backend_handle` because `Arc<dyn EngineBackend>` can't be upcast to `Arc<dyn CompletionBackend>` in Rust's trait-object model. 0.3.4 makes the caller decide.

  Migration:

  ```rust
  // Valkey (completion supported):
  let valkey = Arc::new(ValkeyBackend::connect(backend_config).await?);
  let worker = FlowFabricWorker::connect_with(
      worker_config,
      valkey.clone(),
      Some(valkey),
  ).await?;

  // Backend without completion support:
  let worker = FlowFabricWorker::connect_with(
      worker_config,
      backend,
      None,
  ).await?;
  ```

## [0.3.3] - 2026-04-22

### Fixed

- **`ScannerFilter` now constructible from outside ff-core.** The type
  is `#[non_exhaustive]` to preserve additive compatibility, but 0.3.2
  shipped with no public constructors/setters — external consumers
  (cairn) could only name `ScannerFilter::NOOP`, making the headline
  filtered-completion-subscription surface unusable. Adds
  `ScannerFilter::new()`, `.with_namespace(ns)`, and
  `.with_instance_tag(key, value)` chainable setters. Discovered in
  the 0.3.2 published-artifact smoke
  (`rfcs/drafts/0.3.2-smoke-report.md` Finding 1).
- **`CompletionBackend` now reachable through `FlowFabricWorker`.**
  0.3.2's `worker.backend()` returned `&Arc<dyn EngineBackend>`, and
  `CompletionBackend` is a sibling trait (not a supertrait), so
  consumers could not call `subscribe_completions_filtered` through
  the public worker handle. Adds
  `FlowFabricWorker::completion_backend(&self) -> Option<Arc<dyn CompletionBackend>>`
  populated from the bundled `ValkeyBackend` on the `valkey-default`
  feature. Returns `None` on the `connect_with` path (caller-supplied
  trait object cannot be re-upcast). Discovered in the 0.3.2
  published-artifact smoke
  (`rfcs/drafts/0.3.2-smoke-report.md` Finding 2).

### Changed

- `ff_backend_valkey::ValkeyBackend::from_client_and_partitions` now
  returns `Arc<Self>` (was `Arc<dyn EngineBackend>`). Required so the
  same concrete allocation can back both `EngineBackend` and
  `CompletionBackend` trait-object views. Only caller is ff-sdk's
  `FlowFabricWorker::connect`; any external caller that relied on
  the old signature can reinstate it with a one-line coercion
  (`let backend: Arc<dyn EngineBackend> = arc;`).

### Docs

- Crate-level `ff-sdk` rustdoc quickstart now uses `claim_from_grant`
  (the production path) instead of `claim_next` (which is gated
  behind the default-off `direct-valkey-claim` feature). Smoke
  Finding 3.

## [0.3.2] - 2026-04-22

### Fixed

- Release workflow now publishes `ff-backend-valkey` (9 crates total,
  up from 8). `ff-sdk` depends on `ff-backend-valkey` as an optional
  workspace dep; omitting it from the publish set caused the v0.3.1
  release to partial-publish at `ff-sdk`. `ff-backend-valkey` was
  introduced in RFC-012 Stage 1a (PR #114).

### Notes

- v0.3.1 was yanked after partial-publishing (`ferriskey`, `ff-core`,
  `ff-script`, `ff-observability`, `ff-engine`, `ff-scheduler` —
  6 of 9). Operators should re-resolve against 0.3.2.
- v0.3.0 had been similarly yanked after missing `ff-observability`;
  see the v0.3.1 changelog entry.
- v0.3.2 is the first usable 0.3.x release.

## [0.3.1] - 2026-04-22

### Changed

- Release workflow now publishes `ff-observability` (8 crates total,
  up from 7). `ff-engine` depends on `ff-observability` as a
  workspace dep; omitting it from the publish set caused the v0.3.0
  release to partial-publish.

### Fixed

- `ff_expire_suspension` now emits on `ff:dag:completions` for the
  terminal branches (fail / cancel / expire) when the execution is
  flow-bound. Previously, flow-bound children unblocked only via the
  15 s `dependency_reconciler` safety net.
- `ff_resolve_dependency` child-skip now emits on
  `ff:dag:completions` with `outcome="skipped"` when the skipped
  child is flow-bound. Same latency fix as above for the
  child-skip's own downstream edges.

### Notes

- v0.3.0 was yanked after partial-publishing (`ferriskey`, `ff-core`,
  `ff-script` only). Operators should re-resolve against 0.3.1.
