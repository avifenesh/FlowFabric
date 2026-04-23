# Changelog

All notable changes to FlowFabric are documented here. Format loosely
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- **`ff_attempt_outcome_total` counter metric** (`ff-observability` +
  `ff-backend-valkey`). Labelled by `lane` + `outcome`
  (`ok`|`error`|`timeout`|`cancelled`|`retry`), fired at the
  `EngineBackend` trait boundary after `complete` / `fail` / `cancel`
  succeed on the backend FCALL (including reconciled terminal replays).
  Cardinality bounded at 5 × N lanes (accepted at 5×16=80 per
  Observability RFC prereq #4). Surfaces as panel 11 ("Attempt outcomes
  per lane") on `examples/grafana/flowfabric-ops.json`. Public
  `ff_observability::AttemptOutcome` enum is feature-agnostic so call
  sites stay symmetric whether `observability` is on or off.
- **`EngineBackend::list_flows` — cursor-paginated partition listing (#185).**
  New trait method on `EngineBackend` (core-feature gated) that returns
  a `ListFlowsPage` of `FlowSummary` rows ordered by `FlowId`
  (UUID byte-lexicographic). Callers pass `cursor: None` on the first
  call and forward the returned `next_cursor` to continue iteration;
  `next_cursor: None` signals the partition is exhausted. New public
  types in `ff_core::contracts`: `ListFlowsPage`, `FlowSummary`, and
  `FlowStatus` (an `#[non_exhaustive]` enum mapping the free-form
  `public_flow_state` literal — `open`/`running`/`blocked` collapse to
  `Active`; `completed`/`failed`/`cancelled` project directly; unknown
  literals surface as `Unknown` so callers fall back to
  `describe_flow`). Valkey impl serves the SET-backed listing via
  `SMEMBERS flow_index` + client-side UUID-byte sort + pipelined
  `HGETALL flow_core` per page row; the trait rustdoc documents the
  Postgres implementation pattern (`WHERE flow_id > $cursor
  ORDER BY flow_id LIMIT $limit + 1`) so a future Postgres backend
  serves the same contract natively. `FlowFabricWorker::list_flows`
  wrapper on ff-sdk. Missing `flow_core` for an indexed id surfaces as
  `EngineError::Validation { kind: Corruption, .. }`.
- **`EngineBackend::list_lanes` trait method + `FlowFabricWorker::list_lanes`
  SDK wrapper (#184):** cursor-based pagination over the global lane
  registry (`ff:idx:lanes`). The Valkey impl reads the SET via
  `SMEMBERS`, sorts by `LaneId` name (lexicographic), and returns a
  `limit`-sized page whose `next_cursor` is exclusive; callers loop
  until `next_cursor == None` for the full registry. Gated on the
  `core` feature. `LaneId` gained `PartialOrd` / `Ord` derives so the
  page sort is stable across the trait + SDK surfaces. New
  `ListLanesPage` (`#[non_exhaustive]`) in `ff_core::contracts`.
  Lanes are global (not partition-scoped), so the trait method takes
  no `Partition` argument. Prereq for ff-board's lane-overview panel
  (issue #181 roll-up).
- **`EngineBackend::list_suspended` — cursor-paginated suspended-
  executions listing with reason populated (issue #183).** New
  `core`-gated trait method on `EngineBackend` plus
  `ff_core::contracts::{ListSuspendedPage, SuspendedExecutionEntry}`
  (both `#[non_exhaustive]`). `SuspendedExecutionEntry` carries the
  `reason_code` from `suspension:current` so `ff-board`-style panels
  answer "what is this blocked on?" without a per-row round-trip.
  `reason` is a `String` rather than a closed enum because the
  suspending pipeline accepts free-form reason codes (typical values
  `"signal"`, `"timer"`, `"children"`, `"join"` — but consumers
  embed bespoke codes). Valkey backend impl on cluster routes
  `cluster_scan` through the new hash-tag-aware single-shard path
  (scans only the primary owning the partition tag); standalone
  falls back to the cursor-based `SCAN MATCH` loop. Results merge by
  ascending score with execution id as a lex tiebreak, slice past the
  opaque cursor, and pipeline `HMGET suspension:current reason_code`
  for the returned page. ff-sdk wrapper exposes the trait method
  through `FlowFabricWorker::list_suspended`; the layer scaffolding
  (`HookedBackend`, `PassthroughBackend`) forwards the new op for
  hook observability.
- **`ferriskey::cluster_scan` hash-tag-aware single-shard routing.**
  When the `ClusterScanArgs.match_pattern` embeds a hash tag
  (`{tag}`), the initial scan-state bitmap pre-marks every slot
  scanned except the one owning the tag; the scan touches only that
  primary and finishes once its cursor wraps. Pre-existing
  no-tag behaviour is unchanged. Added `Client::is_cluster()` and a
  typed `Client::cluster_scan()` wrapper that returns
  `(ScanStateRC, Vec<Value>)` directly for Rust callers;
  `ClusterScanArgs` / `ScanStateRC` / `ObjectType` re-exported at the
  crate root.
- **`EngineBackend::list_executions` trait method + `ListExecutionsPage`
  contract (issue #182).** Partition-scoped forward-only cursor
  pagination over the `ff:idx:{p:N}:all_executions` set. Signature is
  `list_executions(partition: PartitionKey, cursor: Option<ExecutionId>,
  limit: usize) -> ListExecutionsPage` gated on the `core` feature so
  Postgres-style backends must honour it. Response carries
  `executions: Vec<ExecutionId>` + `next_cursor: Option<ExecutionId>`
  (exclusive; `None` ⇒ end of stream). Valkey impl reads SMEMBERS,
  lex-sorts on wire form, filters strictly greater than the caller's
  cursor, and trims to `limit` (capped at 1000). `FlowFabricWorker::
  list_executions` thin-forwards onto the trait.
- **Grafana dashboard JSON for operator observability** at
  `examples/grafana/flowfabric-ops.json`. Ten panels covering claim
  latency + rate, lease renewals, worker-at-capacity, admission
  control (quota + budget), scanner cadence + latency, cancel backlog,
  and HTTP request rate + latency. Built against the metrics
  `ff-observability` emits through its OTEL → Prometheus pipeline
  (post #170). Importable via Grafana UI or the `/api/dashboards/db`
  endpoint; see `examples/grafana/README.md`. Positioned as the
  0.5.x operator-visibility alternative while `ff-board` builds out.
- **`ff-observability-http` crate — consumer-side Prometheus `/metrics`
  HTTP endpoint.** New publishable workspace member that bridges
  `ff-observability`'s OTEL + Prometheus registry to an `axum::Router`
  so consumers embedding `ff-sdk` in library mode can expose a scrape
  endpoint without also pulling `ff-server`. Two entry points:
  `router(Arc<Metrics>) -> axum::Router` for merging into an existing
  app, and `serve(addr, Arc<Metrics>)` for a standalone listener. Feature
  model mirrors `ff-observability`: `enabled` off by default, ON
  transitively enables `ff-observability/enabled`. Per the Observability
  RFC adjudication, this ships as a separate crate (not a feature on
  `ff-sdk`) so axum is only pulled into consumers that opt in.
  `ff-server` keeps its own built-in `/metrics` route (PR #94) — this
  crate is for the consumer-in-library-mode use case. Release workflow,
  `docs/RELEASING.md`, and `release.toml` updated for the new
  publish-list entry (guarded by the drift check added in PR #132).
- **Sentry error reporting (`ff-observability/sentry`, `ff-server/sentry`):**
  opt-in Sentry integration for engine-side (`ff-server`, `ff-engine`) and
  consumer-side binaries. New `ff_observability::init_sentry()` reads
  `FF_SENTRY_DSN` / `FF_SENTRY_ENVIRONMENT` / `FF_SENTRY_RELEASE`;
  `ff_observability::sentry_tracing_layer()` composes a `sentry-tracing`
  layer into any `tracing_subscriber::registry()`. Both features are off
  by default and compile out entirely (no `sentry` / `sentry-tracing`
  crates in the dep tree) — orthogonal to `observability` (OTEL metrics).
  `ff-server` calls `init_sentry()` early in `main()` when built with
  `--features sentry`; `FF_SENTRY_DSN` unset is a graceful no-op. See
  `docs/DEPLOYMENT.md` for the env-var table.
- **Observability wiring on `EngineBackend` trait methods (#154):** every
  non-stub `*_impl` in `ff-backend-valkey` now carries
  `#[tracing::instrument(name = "ff.<method>", skip_all, fields(backend,
  execution_id|flow_id, …))]` so consumers can filter logs/spans per
  trait op without pattern-matching free-text. `ValkeyBackend` accepts
  an `Arc<ff_observability::Metrics>` via a new `with_metrics(..)` setter
  plus a `connect_with_metrics(..)` constructor, behind a new
  `observability` feature (off by default; transitively enabled through
  `ff-observability/enabled`). The `inc_lease_renewal` counter fires at
  the `renew` trait-impl boundary — first production emission site
  (previously tests-only).
- **`EngineError::Contextual` + `backend_context` helper (#154):**
  promoted the call-site-label wrap from ff-sdk to
  `ff-core::engine_error` so `ff-backend-valkey` can annotate its
  `EngineBackend` impls with a lightweight context string (e.g.
  `"renew: FCALL ff_renew_lease"`). Wrapping is surgical: only
  `Transport` / `Unavailable` / already-`Contextual` errors get the
  wrapper; typed classifications (`NotFound`, `Validation`,
  `Contention`, `Conflict`, `State`, `Bug`) pass through unchanged so
  consumers `match`-ing on the public variant surface remain
  unaffected. Classification helpers (`class`, `transport_script_ref`,
  `valkey_kind`) transparently descend through the wrapper so
  retry/terminal semantics are preserved.

### Changed

- **BREAKING (unreleased HTTP surface): `GET /v1/executions` reshaped
  to forward-only cursor pagination (issue #182).** Query parameters
  now `partition` (`u16`, required) + optional `cursor`
  (`ExecutionId`, exclusive start) + optional `limit` (capped at
  1000). Response shape swapped from
  `{ executions: [ExecutionSummary], total_returned }` to
  `{ executions: [ExecutionId], next_cursor: ExecutionId | null }`.
  The previous `lane` + `state` filters were dropped; per-execution
  metadata is now fetched via `GET /v1/executions/{id}` or the
  `describe_execution` trait method. The old offset-based endpoint
  was never published on crates.io, so this is unreleased-surface
  churn only. Server::list_executions replaced by
  Server::list_executions_page which mirrors the trait's shape.
- **Hot-path tracing span level downgraded to `debug` (#173):**
  Downgraded 5 hot-path `EngineBackend` impl spans (complete, renew,
  progress, observe_signals, append_frame) from implicit `info` to
  explicit `level = "debug"` to reduce span overhead at production
  `RUST_LOG=info`. See #173 +
  `rfcs/drafts/scenario-5-flame-capture.md` for investigation.
- **v0.4.1 DX polish bundle (#176):** doc-only follow-up to v0.4.0
  consumer validation — six runtime-smoke frictions documented in
  place, no code changes. Rustdoc notes added on
  `CompletionBackend::subscribe_completions_filtered` (completion
  PUBLISH gated on `flow_id` — solo executions never emit),
  `ClaimedTask::update_progress` + `::append_frame` + the same pair
  on the `EngineBackend` trait (cross-reference: `update_progress`
  writes scalar fields on `exec_core`, stream-frame producers must
  use `append_frame`), `ValkeyBackend::connect` (capability erasure
  when returning `Arc<dyn EngineBackend>`; use
  `from_client_partitions_and_connection` to retain
  `CompletionBackend`), `FlowFabricWorker::connect` (rotate
  `WorkerInstanceId` per smoke-script run to avoid the 2×
  lease-TTL SET-NX liveness trap), and `Scheduler::claim_for_worker`
  (lane-FIFO drains prior-run leftovers before fresh submissions —
  recommend a fresh lane per smoke run). `docs/cairn-migration-v0.4.0.md`
  §15 captures the three cross-cutting items (flow_id gating,
  `connect` capability erasure, `Server::start` hard-fail on
  `PartitionConfig` mismatch).

### Fixed

- **ff-script + ff-sdk default-features leak on `ff-core` (#164):**
  completes the feature-gate discipline started in #158. `ff-script`
  and `ff-sdk` no longer depend on `ff-core` with default features
  enabled, so downstream `ff-sdk --no-default-features` builds observe
  only `ff-core/core` — the `streaming`, `suspension`, and `budget`
  gates stay off as RFC-012 §1.3 intends. `ff-sdk`'s `valkey-default`
  feature re-enables the three gated surfaces in lockstep with the
  Valkey-dependent SDK modules (`task`, `worker`, `snapshot`,
  `admin`), preserving the default build. Private feature reshape
  only; no public API change (cargo-semver-checks clean).

## [0.4.0] - 2026-04-23

### Breaking changes

- **Seal `FlowFabricWorker::client()` + migrate stream fns through the
  trait (#87, RFC-012 Stage 1c tranche-4):** the `FlowFabricWorker`
  no longer leaks `ferriskey::Client` on its public surface, and the
  free-fn `read_stream` / `tail_stream` helpers reshape from
  `(&Client, &PartitionConfig, …)` to `(&dyn EngineBackend, …)`.
  - `ff-core`: `EngineBackend` gains `read_stream` + `tail_stream`
    methods, gated on the new `streaming` feature (default-on).
    Backends that honour the `streaming` surface MUST implement
    both.
  - `ff-backend-valkey`: `ValkeyBackend` implements the new trait
    methods behind a mirroring `streaming` feature (default-on).
    The authoritative XRANGE / XREAD BLOCK bodies previously owned
    by `ff-sdk::task::{read_stream,tail_stream}` now live here.
  - `ff-sdk`: `FlowFabricWorker::client()` downgraded from `pub` to
    `pub(crate)` — external consumers cannot obtain the ferriskey
    client through the worker. New `ClaimedTask::read_stream` /
    `ClaimedTask::tail_stream` methods forward through the task's
    backend for task-holders. The free-fn `read_stream` /
    `tail_stream` keep their public names but take `&dyn
    EngineBackend` instead of `&Client` + `&PartitionConfig`;
    validation (count_limit bounds, tail-cursor shape) still
    executes at the SDK edge.
  - `ff-test`: `TestCluster::backend()` accessor added so tests can
    mint an `Arc<dyn EngineBackend>` without spinning a full
    `FlowFabricWorker`.
  - Migration: consumers holding a `ClaimedTask` should call
    `task.read_stream(..)` / `task.tail_stream(..)`; non-task
    callers should build an `EngineBackend` via
    `ValkeyBackend::from_client_and_partitions(..)` and pass
    `&*backend` to the free fns. The `examples/media-pipeline`
    summarize worker exercises the task-method path.

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

- **Snapshot decoders return `EngineError::Validation { Corruption }`
  (RFC-012 Stage 1c T3).** `describe_execution`, `describe_flow`,
  `describe_edge`, `list_incoming_edges`, and `list_outgoing_edges`
  used to surface on-disk-corruption parse failures as
  `SdkError::Config { field, message, .. }`. They now surface as
  `SdkError::Engine(Box<EngineError::Validation { kind:
  ValidationKind::Corruption, detail, .. }>)`. The decoders and
  `FLOW_CORE_KNOWN_FIELDS` moved from `ff-sdk::snapshot` into
  `ff_core::contracts::decode`. Callers matching on the `Config` shape
  must migrate to `Engine(Validation::Corruption)` — see
  `docs/cairn-migration-v0.4.0.md` §6. `SdkError::Config` is retained
  for SDK-side input validation. Breaking error-shape change; pre-1.0
  posture accepts.

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
- **`UsageDimensions` constructor + builder methods (ff-core):**
  `UsageDimensions::new()` plus `with_input_tokens`, `with_output_tokens`,
  `with_wall_ms`, `with_dedup_key` chaining setters. Mirrors the
  `ScannerFilter::with_*` precedent — `UsageDimensions` is
  `#[non_exhaustive]` so external crates cannot struct-literal
  construct; `new()` + `with_*` is now the supported construction
  path. Purely additive; existing `UsageDimensions::default()` call
  sites continue to work.
- **DX polish — re-exports + `ScannerFilter::with_namespace` ergonomics
  (HHH v0.3.4 re-smoke):** four additive papercut fixes for cairn's
  migration path.
  - `ff_core::ScannerFilter` re-exported at the crate root (was only
    reachable via `ff_core::backend::ScannerFilter`).
  - `ff_core::backend::Namespace` re-exported from `ff_core::types`
    so consumers already scoped to `ff_core::backend::*` don't need
    a second `use` crossing into `ff_core::types`. `Namespace`
    remains canonically exported at `ff_core::types::Namespace`; this
    is purely additive.
  - `ff_backend_valkey::BackendConfig` re-exported from
    `ff_core::backend::BackendConfig` so single-crate consumers of
    `ff_backend_valkey` don't need to also name `ff_core::backend`.
  - `ScannerFilter::with_namespace` now accepts
    `impl Into<Namespace>` (was typed `Namespace`). Non-breaking:
    existing `Namespace::new("t")`-style call sites still compile
    via the identity `From<Namespace> for Namespace` impl, and `&str`
    / `String` now work directly via `string_id!`'s `From` impls.

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
- **`ValkeyBackend::describe_execution` and `::describe_flow` are now
  wired (RFC-012 Stage 1c T3).** Previously returned
  `EngineError::Unavailable`. Implementation uses the same HGETALL
  pipeline ff-sdk owned pre-T3, now routing every parse failure
  through the relocated `ff-core` decoders. `ff-sdk`'s
  `FlowFabricWorker::describe_execution` / `describe_flow` /
  `list_*_edges` collapse to thin forwarders over the trait.
- **`BackendTimeouts.request` now honored (#139).** `ValkeyBackend::connect`
  previously dropped `config.timeouts` on the floor, dialing via URL-based
  `Client::connect` / `connect_cluster`; the field had no observable effect.
  Migrated to `ferriskey::ClientBuilder` so `BackendTimeouts.request` threads
  through to `ClientBuilder::request_timeout` (cluster + TLS flags also move
  to the builder; no behavior change for those). `None` preserves the
  ferriskey default. Behavior-visible: consumers that set a custom
  `request` timeout will now see it enforced.

### Infrastructure

- Scenario 4 bench methodology: switched to N=5 multi-sample runs with
  reported mean + stdev for statistical honesty; single-sample numbers
  previously hid run-to-run variance (#140).
- Refreshed Scenario 4 baseline at v0.1.0 + HEAD under the N=5
  methodology so regression tracking has a current reference point
  (#144).
- Scenario 4 `apalis` comparison now uses `apalis-workflow` DAG rather
  than hand-rolled chaining, aligning the comparand with apalis's
  intended DAG surface (#51, #148).

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
