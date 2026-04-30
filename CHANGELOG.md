# Changelog

All notable changes to FlowFabric are documented here. Format loosely
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Changed

- **`WorkerConfig.backend` is now `Option<BackendConfig>` (SC-10
  ergonomics follow-up, cairn-reported via
  `feedback_sdk_reclaim_ergonomics.md` Finding 2).** Pre-v0.13 the
  field was eager `BackendConfig`, but only
  [`FlowFabricWorker::connect`] consumed it; the backend-agnostic
  [`FlowFabricWorker::connect_with`] path ignored it entirely,
  forcing `connect_with` callers to carry a placeholder
  `BackendConfig::valkey(...)` just to satisfy the struct literal.
  The v0.12 SC-10 `incident-remediation` example surfaced this as a
  rough edge for the SQLite supervisor path. Semantics:
  - `Some(cfg)` + `connect` — unchanged (dial using `cfg`).
  - `None` + `connect` — rejected with `SdkError::Config` (the
    URL-based path needs a `BackendConfig` to dial).
  - `None` + `connect_with` — clean, no placeholder needed.
  - `Some(cfg)` + `connect_with` — accepted, logs a `tracing::warn!`
    because `cfg` is ignored (the injected backend is authoritative).
  Existing `connect` callers migrate by wrapping their existing
  `BackendConfig` in `Some(...)`; `connect_with` callers can drop
  the placeholder and pass `None`. See
  [`docs/CONSUMER_MIGRATION_0.13.md`](docs/CONSUMER_MIGRATION_0.13.md)
  for the before/after snippet.

### Added

- **PR-7b Cluster 2b-B: flow projector + retention trimmer scanners
  trait-routed (cairn #436).** Two new `EngineBackend` trait methods —
  `project_flow_summary(partition, flow_id, now_ms)` and
  `trim_retention(partition, lane_id, retention_ms, now_ms, batch_size, filter)`
  — route the two remaining non-FCALL scanners (`flow_projector` and
  `retention_trimmer`) off the raw `ferriskey::Client` surface.
  Valkey lifts the pre-PR-7b Rust-composed paths verbatim (SRANDMEMBER
  + cross-partition HGET aggregation for flow projection; ZRANGEBYSCORE
  + per-execution cascade-delete for retention). Postgres aggregates
  member `public_state` via `COUNT(*) FILTER` on `ff_exec_core` +
  INSERT...ON CONFLICT DO UPDATE into `ff_flow_summary` (new migration
  `0019_flow_summary`), and cascades retention DELETEs across every
  execution-scoped sibling table inside one transaction. SQLite returns
  `Unavailable` for both per RFC-023 Phase 3.5 (neither is required for
  single-tenant local deployments). Both scanners retain a test-only
  direct-client fallback path for instantiations without a backend.
- **`examples/external-callback` — SC-09 headline demo for the v0.13
  waitpoint consumer surface.** A workflow suspends against an
  HMAC-signed pending waitpoint, an asynchronous external actor POSTs
  the token back, a signal-bridge shim authenticates the token via
  [`EngineBackend::read_waitpoint_token`] (#434), and
  [`FlowFabricWorker::deliver_signal`] resumes the flow. Exercises
  the four v0.13 surfaces end-to-end on a zero-infra SQLite backend
  (`FF_DEV_MODE=1 cargo run -p external-callback -- --backend sqlite`):
  `create_waitpoint` (#435), the backend-agnostic
  `FlowFabricAdminClient::connect_with` facade (#432),
  `WorkerConfig.backend: Option<BackendConfig>` (#448), and the HMAC
  verify path on `read_waitpoint_token`. Includes a tamper-rejection
  branch (flip one hex char, keep the `kid:` prefix) to demonstrate
  constant-time token compare. Production migration reference:
  [`docs/CONSUMER_MIGRATION_0.13.md`](docs/CONSUMER_MIGRATION_0.13.md).
  Distinct from the synchronous `deploy-approval` operator-gate and
  the `incident-remediation` (SC-10) reclaim-handoff scenarios —
  SC-09 is the asynchronous-external-actor contract cairn's
  signal-bridge architecture targets. ~410 LoC Rust + README.
- **PR-7b Cluster 3: cancel-family scanners trait-routed (cairn #436).**
  Two new `EngineBackend` trait methods plus the
  `SiblingCancelReconcileAction` enum on
  `ff_core::engine_backend`: `drain_sibling_cancel_group` (RFC-016
  Stage C — atomic SREM + HDEL for a drained sibling-cancel tuple)
  and `reconcile_sibling_cancel_group` (RFC-016 Stage D — the
  three-way Invariant-Q6 disposition). Valkey wires both to the
  existing `ff_drain_sibling_cancel_group` and
  `ff_reconcile_sibling_cancel_group` FCALLs; Postgres + SQLite
  default to `Unavailable` (PG's
  `reconcilers::edge_cancel_{dispatcher,reconciler}::*_tick`
  already owns the equivalent batched SQL path; the Valkey-shaped
  per-tuple call is not a cairn-consumer surface). The three
  cancel-family scanners — `cancel_reconciler`,
  `edge_cancel_dispatcher`, `edge_cancel_reconciler` — now route
  their FCALL bodies through `EngineBackend::cancel_execution` +
  `ack_cancel_member` (verified semantic match: both existing trait
  methods already FCALL the same Lua fns with byte-identical
  KEYS/ARGV as the scanner internals) plus the two new
  sibling-cancel methods. Legacy direct-FCALL paths remain as a
  fallback for construction sites that do not supply a backend
  (test harnesses + `EdgeCancelDispatcher::new` /
  `EdgeCancelReconciler::new`).
- **PR-7b Cluster 4: trait-routed completion listener (cairn #436).**
  New `EngineBackend::cascade_completion(&CompletionPayload)` trait
  method captures the full post-completion cascade behind
  `Arc<dyn EngineBackend>`, with a typed
  `ff_core::contracts::CascadeOutcome` return (`#[non_exhaustive]`;
  counters `resolved` + `cascaded_children` + `synchronous`). Valkey
  impl (`ff_backend_valkey::cascade::run_cascade`) moves the
  pre-existing `ff-engine::partition_router::dispatch_dependency_resolution`
  body behind the trait — same KEYS[14]+ARGV[5] FCALL, same recursive
  `child_skipped` cascade up to `MAX_CASCADE_DEPTH=50`, preserved
  exactly. Postgres impl resolves the payload to its
  `ff_completion_event.event_id` and invokes Wave-5a
  `dispatch_completion`. `spawn_dispatch_loop` signature changes from
  `(router, client, stream, shutdown)` to
  `(backend: Arc<dyn EngineBackend>, stream, shutdown)`;
  `run_completion_listener_postgres` shim grows an `engine_backend`
  argument. **Documented timing divergence** (Valkey-sync vs PG-outbox)
  in `docs/POSTGRES_PARITY_MATRIX.md` row 5c +
  `docs/CONSUMER_MIGRATION_0.13.md` — architectural, not a parity gap.
- **PR-7b Cluster 1: foundation scanner operations on `EngineBackend`
  (cairn #436).** Five new trait methods + `ExpirePhase` enum on
  `ff_core::engine_backend::EngineBackend` for the six simple-expiry
  scanners: `mark_lease_expired_if_due`, `promote_delayed`,
  `close_waitpoint`, `expire_execution(phase: ExpirePhase)` (shared
  between attempt-timeout and execution-deadline scanners), and
  `expire_suspension`. Valkey impl wraps the existing
  `ff_mark_lease_expired_if_due` / `ff_promote_delayed` /
  `ff_close_waitpoint` / `ff_expire_execution` / `ff_expire_suspension`
  FCALLs; Postgres wires `mark_lease_expired_if_due`,
  `expire_execution(AttemptTimeout)`, and `expire_suspension` to
  per-row helpers extracted from the existing batch reconcilers in
  `ff_backend_postgres::reconcilers::*` (`promote_delayed`,
  `close_waitpoint`, and `expire_execution(ExecutionDeadline)` remain
  at the `EngineError::Unavailable` default — RFC-020 Wave 9 schema
  scope); SQLite stays on the trait defaults (per RFC-023 §4.1 the
  SQLite backend hosts its own reconciler supervisor, not the
  engine's scanner loop). `scanner::should_skip_candidate` dispatches
  the namespace check through a new
  `EngineBackend::get_execution_namespace` single-field point-read
  trait method (preserves the 1-HGET cost contract), and the tag
  check through `EngineBackend::get_execution_tag`. Engine-side:
  each of the 11 scanners that use `should_skip_candidate` grew a
  `backend: Option<Arc<dyn EngineBackend>>` field plus a
  `with_filter_and_backend(..)` constructor; the 6 Cluster-1 scanner
  bodies route their operation FCALL through the trait when a
  backend is wired. Unblocks PR-7b Clusters 2 + 3 (reconcilers +
  cancel-family) to land in parallel.
- **`EngineBackend` service-layer typed FCALL surface (cairn #389).**
  Six new trait methods let control-plane consumers (cairn et al.)
  dispatch against `(execution_id, fence)` tuples instead of requiring
  a worker `Handle`, eliminating the raw `ferriskey::Value` +
  `check_fcall_success` + `parse_*` pattern cairn had on 15+ sites in
  `valkey_control_plane_impl.rs`:
  `complete_execution`, `fail_execution`, `renew_lease`,
  `resume_execution`, `check_admission`, `evaluate_flow_eligibility`.
  Args/Result types already existed in `ff_core::contracts`; the gap
  was the trait-method surface that lets cairn drop its direct
  `ferriskey` dep and unblock the Postgres-backend migration. Each
  method defaults to `EngineError::Unavailable` so out-of-tree
  backends keep compiling; Valkey overrides all six with thin
  delegates to the existing `ff_script::functions::*` wrappers.
  `check_admission` takes `quota_policy_id: &QuotaPolicyId` and
  `dimension: &str` as trait-method arguments alongside
  `CheckAdmissionArgs` — quota keys live on the `{q:<policy>}`
  partition that cannot be derived from `execution_id`, and
  widening the `CheckAdmissionArgs` struct would have been a semver
  break for existing struct-literal callers. Empty `dimension` falls
  back to `"default"` to match cairn's pre-migration default.
  Postgres + SQLite keep the
  `Unavailable` default pending follow-up parity work (tracked in
  `POSTGRES_PARITY_MATRIX.md`); same staging precedent as
  `issue_reclaim_grant` / `reclaim_execution`.
- **Backend-agnostic `FlowFabricAdminClient` facade (SC-10 ergonomics
  follow-up, v0.13).** `FlowFabricAdminClient` now supports both HTTP
  (`new` / `with_token` — existing) and embedded
  (`connect_with(Arc<dyn EngineBackend>)` — new) transports behind
  one public method surface. `claim_for_worker`,
  `issue_reclaim_grant`, and `rotate_waitpoint_secret` dispatch to
  the matching `EngineBackend` trait method on the embedded path.
  Consumers running under `FF_DEV_MODE=1` + SQLite no longer need to
  drop down to trait-direct `backend.issue_reclaim_grant(...)` — the
  full SDK admin surface is reachable without standing up an
  `ff-server`. `EngineError::Unavailable` from the backend maps to
  `SdkError::AdminApi { status: 503, kind: Some("unavailable"), ... }`
  so callers see a uniform admin-error surface regardless of
  transport. `examples/incident-remediation` updated to drive the
  supervisor's reclaim path through the facade. See
  [`docs/CONSUMER_MIGRATION_0.13.md`](docs/CONSUMER_MIGRATION_0.13.md).
- **`ff-backend-postgres`: `EngineBackend::create_waitpoint` implemented.**
  Completes cairn #435 — the previously stubbed (`EngineError::Unavailable`)
  caller-initiated waitpoint path now mints a fresh `WaitpointId`, signs
  a token under the active HMAC kid, and INSERTs a `pending` row into
  `ff_waitpoint_pending`. Behaviour mirrors `ff-backend-sqlite`'s
  `create_waitpoint_impl` and Valkey's `ff_create_pending_waitpoint`:
  state stays `pending` (only `suspend` activates); tokens bind to
  `"{execution_id}:{waitpoint_id}"` so the existing `deliver_signal`
  verify path accepts them unchanged; `waitpoint_key` is not an
  idempotency key — every call mints a distinct `WaitpointId`. Unblocks
  approval-flow control-plane paths on Postgres deployments.
- **`EngineBackend` tag point-writes + reads (issue #433).** Four new
  trait methods land `set_execution_tag`, `set_flow_tag`,
  `get_execution_tag`, `get_flow_tag` so operator/control-plane
  tooling can write caller-namespaced tags (e.g. `cairn.session_id`,
  `cairn.archived`) through `Arc<dyn EngineBackend>` on any backend
  instead of downcasting to `ValkeyBackend` + direct `HSET`. Backed by
  the existing `ff_set_{execution,flow}_tags` Lua contracts on Valkey
  and `raw_fields` JSON(B) upserts on Postgres/SQLite. New trait-side
  `ff_core::engine_backend::validate_tag_key` helper enforces the
  `^[a-z][a-z0-9_]*\.[a-z0-9_][a-z0-9_.]*$` namespace regex on every
  backend so Postgres/SQLite reject the same keys as Valkey.
  Validation runs end-to-end: the full key (not just the namespace
  prefix) must be `[a-z0-9_.]` — a tightening over the
  prefix-only check shipped in the first pass, which would have
  silently accepted keys containing spaces, uppercase suffix chars,
  or quotes that break SQLite JSON-path quoting. Read-side
  missing-row collapses to `Ok(None)` (matches Valkey's `HGET`
  semantics); write-side missing entity surfaces as
  `EngineError::NotFound`. Cross-backend parity tests live at
  `crates/ff-test/tests/matrix_engine_backend_tags.rs` (Valkey + PG)
  and `crates/ff-backend-sqlite/tests/engine_backend_tags.rs`
  (SQLite). **SQLite JSON-path note**: all four paths quote the key
  segment (`$.tags."<key>"` / `$."<key>"`) so dotted namespaced keys
  like `cairn.session_id` land as a single flat member rather than
  nested path segments — matching the Postgres `jsonb_set(...,
  ARRAY['tags', $key::text], ...)` shape and the
  `$.tags."cairn.instance_id"` pattern already used by existing
  SQLite operator queries. **Valkey flow-tag migration note**:
  `set_flow_tag` on Valkey routes through the existing
  `ff_set_flow_tags` FCALL, which on the first write against a given
  flow scans `flow_core` for pre-58.4 inline namespaced fields,
  moves them to the dedicated `:tags` hash, and stamps a
  `tags_migrated=1` sentinel so subsequent writes are O(1). This is
  backward-compat migration, not a new behaviour — existing consumers
  that only read tags via `ff_set_execution_tags` are unaffected;
  consumers that were reading flow tags directly off `flow_core`
  (HGETALL) should migrate to `EngineBackend::get_flow_tag`.
  No storage migrations — existing `raw_fields` JSONB already holds
  tags on PG/SQLite.

## [0.12.0] - 2026-04-28

### Changed

- **Agnostic-SDK PR-5.5 — `ClaimedTask` + `claim_from_grant` /
  `claim_via_server` / `claim_resumed_execution` /
  `read_execution_context` ungated.** The SDK's worker hot paths are
  no longer `#[cfg(feature = "valkey-default")]`-gated at the method
  level; they now route exclusively through the `EngineBackend`
  trait and compile under `--no-default-features, features =
  ["sqlite"]`. The `ClaimedExecution` and `ClaimedResumedExecution`
  contract structs gain a new `handle:
  ff_core::backend::Handle` field populated by the owning backend at
  claim time; `ClaimedTask::new` caches the handle and clones it
  into each per-op trait forwarder (replaces the pre-PR
  `ValkeyBackend::encode_handle` synthesis call that hardcoded the
  Valkey backend tag). `BackendTag`, `HandleKind`, `HandleOpaque`,
  and `Handle` gain `#[derive(Serialize, Deserialize)]` to support
  the new contract field. PG and SQLite backends populate
  `claim_resumed_execution.handle` with a real backend-tagged
  encoded handle; `claim_execution` on those backends still returns
  `EngineError::Unavailable` at runtime per
  `project_claim_from_grant_pg_sqlite_gap.md` (ungate is
  compile-surface; runtime coverage on PG/SQLite remains the
  scheduler-routed `claim_via_server` path). `ClaimedTask::read_stream`
  / `tail_stream` / `tail_stream_with_visibility` remain
  `valkey-default`-gated — the backing trait methods require
  ff-core's `streaming` feature which the `sqlite` feature set does
  not activate today. See
  [`docs/CONSUMER_MIGRATION_0.12.md`](docs/CONSUMER_MIGRATION_0.12.md)
  §8 for details.

### Added

- **`examples/incident-remediation` — SC-10 headline example for the
  RFC-024 lease-reclaim consumer surface.** Two-responder pager-death
  handoff: Responder A claims an incident, drops mid-flow; a
  supervisor issues a `ReclaimGrant`; Responder B picks up via
  `FlowFabricWorker::claim_from_reclaim_grant` and completes. A
  second incident is run past `max_reclaim_count` to demonstrate the
  `ReclaimExecutionOutcome::ReclaimCapExceeded` escalation branch.
  Runs end-to-end under `FF_DEV_MODE=1 cargo run -p
  incident-remediation -- --backend sqlite` with zero external infra
  (companion to `examples/ff-dev`'s RFC-023 SQLite dev-mode
  showcase). Valkey + Postgres dispatch paths compile + connect but
  defer the full loop to deployments with a live scheduler +
  scanner supervisor — see the example README for setup notes.
  Cross-links `docs/CONSUMER_MIGRATION_0.12.md` §7 as the production
  migration reference.
- **RFC-024 PR-G — SDK consumer surface for lease-reclaim (closes
  #371 end-to-end).** Adds the three consumer-facing surfaces that
  cairn-fabric (and any pull-mode consumer) calls to recover from
  `lease_expired`. Backend impls landed earlier in PR-D/E/F; this
  PR wires them through to the SDK + HTTP.
  - `ff-sdk::FlowFabricAdminClient::issue_reclaim_grant(&self,
    execution_id, IssueReclaimGrantRequest) ->
    Result<IssueReclaimGrantResponse, SdkError>` — HTTP admin-surface
    wrapper over `POST /v1/executions/{id}/reclaim`. `status`-
    discriminated response enum with `Granted` /
    `NotReclaimable` / `ReclaimCapExceeded` variants; consumers
    lift `Granted` into `ff_core::contracts::ReclaimGrant` via
    `IssueReclaimGrantResponse::into_grant`. `valkey-default`-gated
    (lives alongside the existing reqwest-based admin surface).
  - `ff-sdk::FlowFabricWorker::claim_from_reclaim_grant(&self,
    ReclaimGrant, ReclaimExecutionArgs) ->
    Result<ReclaimExecutionOutcome, SdkError>` — **backend-agnostic**
    (no cfg gate). Dispatches through
    `EngineBackend::reclaim_execution` on whichever backend the
    worker was connected with. Compiles + runs under
    `--no-default-features, features = ["sqlite"]`; companion
    compile-anchor in `crates/ff-sdk/tests/rfc024_sdk.rs` pins the
    signature so accidental cfg-gating regressions fail the build.
  - `ff-server`: new `POST /v1/executions/{id}/reclaim` route.
    Dispatches through the `EngineBackend` trait's
    `issue_reclaim_grant`; all three in-tree backends
    (Valkey / Postgres / SQLite) implement it at v0.12.0. 64 KiB
    `BODY_LIMIT_CONTROL` cap; validates `worker_id` /
    `worker_instance_id` / `lane_id` the same way the existing
    `/v1/workers/{id}/claim` handler does, rejects zero-or-over-
    60s `grant_ttl_ms`.
- `docs/CONSUMER_MIGRATION_0.12.md` §7 — comprehensive RFC-024
  consumer-migration section: breaking-change inventory from PR-B/C,
  additive surfaces from PR-G, full cairn F64-bridge-retry →
  reclaim migration snippet, handle-kind awareness note, and the
  `ReclaimGrant` vs `ResumeGrant` rename clarification for
  migrators.

- `examples/ff-dev/` — RFC-023 Phase 4 headline example for the
  SQLite dev-only backend (v0.12.0 release target). ~200-line
  consumer-facing demo that drives the `EngineBackend` trait directly
  against an in-memory `SqliteBackend`: `create_flow` +
  `create_execution` + `claim` + `complete` + Wave-9 admin
  (`change_priority`, `cancel_execution`) + `read_execution_info` +
  RFC-019 `subscribe_completion` surface. No Docker, no ambient
  services; runs as `FF_DEV_MODE=1 cargo run --bin ff-dev`. Companion
  `scripts/smoke-sqlite.sh` wraps the example as a release-gate smoke
  (RFC-023 §9) and is wired into the `matrix-tests-complete` required
  check via `.github/workflows/matrix.yml`. The example follows the
  RFC-023 §4.7.1 cairn-canonical shape (no ff-sdk / ferriskey in the
  dep graph) and honours the `FF_DEV_MODE=1` production guard.

### Fixed

- `ferriskey` (cluster): error-driven slot-refresh sites now use
  `RefreshPolicy::NotThrottable`. Previously, the `PollFlushAction::
  RebuildSlots` path (triggered by server-side signals such as
  `READONLY`, `MOVED`, `TRYAGAIN`) and the post-failure retry path in
  `poll_recover` both used `Throttable`, so a refresh that fired inside
  the 15s throttle window was silently skipped — leaving the client
  stuck re-observing the same server error until the window elapsed.
  Periodic (time-driven) refreshes remain `Throttable`. The throttle
  skip is now logged at `warn!` (was `debug!`) with the trigger,
  throttle window, and time-since-last-refresh, so production traces
  make this state visible without a log-level bump. New pure helper
  `decide_should_refresh` + `throttle_tests` pin the contract.
- `ff-script`: unit tests pin `is_permanent_load_error` classification
  for the transient server-directed error families (`READONLY`,
  `MOVED`, `TRYAGAIN`, `CLUSTERDOWN`) — all four must remain transient
  so the loader's retry path keeps working on replica-claim races and
  resharding. Regression guard against substring-match drift in the
  permanent-vs-transient classifier.
- `ff-core`: `engine_backend.rs` imports of `SummaryDocument` and
  `TailVisibility` are now `#[cfg(feature = "streaming")]`-gated to
  match the trait methods that reference them — removes a
  `-D warnings` failure on the default (non-streaming) build.
- `ff-backend-sqlite`: `backend.rs` `:memory:` URI rewrite comment
  corrected to match the code (`file::memory:?cache=shared` — no
  `uri=true` query param; sqlx infers URI mode from the `file:`
  prefix). Doc-only.
- `ff-backend-sqlite`: `tests/hot_path.rs` comment on
  `claim_happy_path_mints_handle_and_transitions_state` clarified —
  removed the inaccurate "column remained at `'waiting'`" literal
  (the seed row uses `'pending'`); now describes the general bug
  (claim path did not update `public_state` at all) without naming
  a specific pre-claim literal. Doc-only.
- `scripts/smoke-sqlite.sh`: guard leg now distinguishes a clean
  refusal from a crash/link-error / empty-output case by asserting
  (1) non-zero exit, (2) non-empty output, (3) refusal message
  present — each as an independent check with its own failure
  message. Previously a `cargo run` that produced no output could
  in principle false-pass the inverted-grep assertion.
- `ff-backend-sqlite`: `Cargo.toml` now correctly enables
  `libsqlite3-sys/bundled` via an explicit dep so the backend actually
  links a bundled SQLite, matching the long-standing "Bundled SQLite
  is deliberately selected" comment. sqlx 0.8 has no `sqlite-bundled`
  feature alias, so the previous config silently fell through to
  system-linked `libsqlite3-sys` and the comment did not hold.
  Deterministic CI builds + distro independence per RFC-023 §7.1
  (JSON1 + RETURNING rely on SQLite >= 3.35). `examples/ff-dev` now
  asserts the `SELECT sqlite_version()` floor at startup and
  `scripts/smoke-sqlite.sh` logs the bundled version in CI output.
- `ff-backend-postgres`: `ff_attempt.outcome` is now cleared on the
  exec-cancel paths that previously left it stale — `cancel_flow`
  member loop (`src/flow.rs`) and `exec_core::cancel` (the
  `Handle`-level cancel). The existing `operator::cancel_execution`
  path already cleared outcome on live leases; this extends the
  invariant to the remaining cancel sites so a post-cancel
  `read_execution_info` never surfaces a stale `retry` /
  `interrupted` terminal-outcome on a cancelled row.
  `ff-backend-sqlite` gets the parallel fix on the `cancel_flow`
  member loop. Closes #355.
- `ff-backend-postgres`, `ff-backend-sqlite`: added
  `ff_exec_core.started_at_ms` (migration 0016) as a set-once
  first-claim timestamp column. Populated by the claim + resume-claim
  paths via `COALESCE(started_at_ms, now)` so reclaim + retry never
  overwrite the original value. The Wave-9 Spine-B
  `read_execution_info` read path drops the LATERAL join (Postgres) /
  correlated subquery (SQLite) on `ff_attempt.started_at_ms` and reads
  the column directly. Migration 0016 backfills existing rows from
  `MIN(ff_attempt.started_at_ms)` per execution. Migration number
  0015 is reserved for RFC-024 (claim-grant table); if the RFC-024
  impl PR lands first the two renumber independently — 0016 stays
  additive and safe. Closes #356.
- `ff-backend-postgres`: first-claim path now writes
  `public_state = 'running'` on `ff_exec_core` for parity with the
  resume-claim path (`src/suspend_ops.rs`) and the SQLite first-claim
  write landed in PR #392. Prior to this fix the column remained at its
  create-time `'waiting'` literal after a PG first-claim, so Spine-B
  `read_execution_info` readers (and any direct `SELECT public_state`)
  saw the wrong column state until the read-boundary
  `normalise_public_state` adapter masked it. Regression coverage in
  `crates/ff-backend-postgres/tests/wave9_followups.rs`
  (`first_claim_writes_public_state_running`).

### Documentation

- `rfcs/RFC-020-postgres-wave-9.md` §4.1 (Revision 8): corrected the
  earlier "maps directly" claim for `ff_exec_core.lifecycle_phase` +
  sibling state columns. Postgres legitimately encodes
  `(phase × eligibility × terminal-outcome)` in a richer private
  alphabet than the canonical `ff_core::state` enums (`cancelled`,
  `released`, `pending_claim`, bare `running`, `blocked` are valid
  column literals but not enum members). The `normalise_*` shim in
  `crates/ff-backend-postgres/src/exec_core.rs` is now documented as
  the read-boundary adapter (Option B per owner decision on #354),
  not a transient inconsistency; a corresponding module-level doc on
  `exec_core.rs` spells out the invariant for new read + write paths.
  Closes #354.

### Added

- **RFC-024 PR-F — Valkey `issue_reclaim_grant` + `reclaim_execution`
  real impls + Lua ARGV[9] threading (continues #371).** Replaces the
  PR-B+C `EngineError::Unavailable` default with direct FCALL forwards
  on the Valkey backend. Scope: `ff-backend-valkey` +
  `ff-script/flowfabric.lua` only; Postgres (PR-D) + SQLite (PR-E)
  land separately.
  - `ValkeyBackend::issue_reclaim_grant` — forwards to
    `ff_issue_reclaim_grant` (KEYS[3] + ARGV[9] unchanged). Maps the
    Lua `execution_not_reclaimable` + `capability_mismatch` err codes
    to `IssueReclaimGrantOutcome::NotReclaimable { detail, .. }`;
    success mints a `ReclaimGrant` carrying the server-authoritative
    `expires_at_ms` (returned by Lua from `TIME + grant_ttl_ms`) +
    the execution's lane (per RFC-024 §3.1 lane asymmetry with
    `ResumeGrant`).
  - `ValkeyBackend::reclaim_execution` — forwards to
    `ff_reclaim_execution` with new ARGV[9] threading the caller's
    `max_reclaim_count.unwrap_or(1000)` (Rust-surface default per
    RFC-024 §4.6). Success mints a `Handle { kind: Reclaimed, .. }`
    via `handle_codec::encode_handle(.., HandleKind::Reclaimed)`;
    Lua err codes map to typed outcomes: `invalid_claim_grant` →
    `GrantNotFound`, `execution_not_reclaimable` → `NotReclaimable`,
    `max_retries_exhausted` → `ReclaimCapExceeded` (with the
    authoritative post-policy-override cap surfaced in
    `reclaim_count`).
  - `ff-script: ff_reclaim_execution` Lua primitive gains optional
    `ARGV[9] = default_max_reclaim_count`. Resolution order is
    (1) per-execution policy override at `<core>:policy`
    (`policy.max_reclaim_count`), (2) caller-supplied ARGV[9],
    (3) legacy fallback `"100"`. Pre-RFC call sites (8-ARGV) keep
    working unchanged — the `or "100"` default preserves pre-RFC
    behaviour exactly. ARGV[9] is now asserted numeric so a
    caller-side bug surfaces loudly instead of crashing on the
    `>=` comparison.
  - `ff-backend-valkey: valkey_supports_base().issue_reclaim_grant = true`
    — flips the new RFC-018 capability flag (see `### Changed`) so
    snapshot consumers see the new surface available on Valkey.
  - Integration tests: `crates/ff-backend-valkey/tests/rfc024_reclaim.rs`
    adds six `#[ignore]`-gated scenarios (happy-path /
    grant-not-found / nonexistent-execution / granted-on-expired /
    server-authoritative `expires_at_ms` / policy-override cap
    surfaced on `ReclaimCapExceeded`) against a live Valkey.
    `tests/capabilities.rs` adds an assertion that
    `supports.issue_reclaim_grant` is `true` on a dialed Valkey
    backend.
- **RFC-024 PR-D — Postgres `issue_reclaim_grant` + `reclaim_execution`
  real impls + migration 0017 (continues #371).** Replaces the
  PR-B+C `EngineError::Unavailable` default with production SQL
  bodies on the Postgres backend. Scope: `ff-backend-postgres`
  only; SQLite (PR-E) + Valkey (PR-F) land separately.
  - `crates/ff-backend-postgres/migrations/0017_claim_grant_table.sql`
    — new 256-way HASH-partitioned `ff_claim_grant` table with
    `kind IN ('claim','reclaim')` discriminator, JSONB
    `worker_capabilities`, per-grant TTL columns + indexes on
    `(partition_key, execution_id)` and `expires_at_ms`. Adds
    `ff_exec_core.lease_reclaim_count INTEGER NOT NULL DEFAULT 0`
    for the RFC-024 §4.6 1000-cap enforcement. Backfills existing
    `raw_fields.claim_grant` JSON entries into `kind='claim'`
    rows (grant_id = sha256(grant_key)). `backward_compatible = true`;
    the legacy JSON column is left in place for one release per
    RFC-024 §5 / §10 (a follow-up cleanup strips the residue
    once rolling-deploy windows close).
  - `crates/ff-backend-postgres/src/claim_grant.rs` — new module
    housing the `ff_claim_grant` CRUD + the two RFC-024
    impls. SERIALIZABLE + 3-attempt retry wrapper per the
    Wave-9 `operator::cancel_execution_impl` pattern.
  - `PostgresBackend::issue_reclaim_grant` — validates
    `lifecycle_phase = 'active'` + (ownership_state in
    `{'lease_expired_reclaimable','lease_revoked'}` OR the
    attempt's lease has expired / is NULL) under `FOR NO KEY
    UPDATE`; pre-checks the 1000-cap; signs a reclaim grant via
    the existing `ff_waitpoint_hmac` keystore; inserts
    `kind='reclaim'` row. Returns `Granted(ReclaimGrant)` /
    `NotReclaimable` / `ReclaimCapExceeded`.
  - `PostgresBackend::reclaim_execution` — locks the reclaim
    grant row under `FOR UPDATE`, validates grant worker_id
    match (RFC-024 §4.4 — `worker_instance_id` intentionally
    informational only), validates TTL, inserts a NEW
    `ff_attempt` row with `attempt_index = current + 1`
    (`raw_fields.attempt_type = 'reclaim'`) matching Valkey
    `ff_reclaim_execution` semantics, marks the prior attempt
    `outcome = 'interrupted_reclaimed'`, bumps `exec_core.attempt_index`
    + `lease_reclaim_count`, consumes the grant row, emits a
    RFC-019 `reclaimed` lease event, and mints a
    `Handle { kind: Reclaimed, lease_epoch: 1 }`. Enforces the
    caller-supplied `max_reclaim_count` (default 1000); on
    exceed: transitions to `terminal_failed` + consumes the
    grant + returns `ReclaimCapExceeded`.
  - `crates/ff-backend-postgres/tests/rfc024_reclaim.rs` — 7
    integration scenarios (happy paths for both methods,
    wrong-phase, cap-exceeded at issuance, worker-id mismatch,
    TTL-expired grant, cap-exceeded at reclaim, scheduler
    write-through verification).

- **RFC-024 PR-E — SQLite `issue_reclaim_grant` + `reclaim_execution`
  impls + migration 0017 (advances closure of #371).** SQLite wiring
  for the RFC-024 lease-reclaim path — the PR-B+C trait defaults are
  replaced with real bodies on the SQLite backend. Parallel to PR-D
  (Postgres); different backend crate, disjoint.
  - `crates/ff-backend-sqlite/migrations/0017_claim_grant_table.sql`
    — new `ff_claim_grant` table (SQLite-dialect port of PR-D's
    Postgres migration; BLOB for raw bytes, TEXT JSON for
    capabilities, no partitioning per RFC-023 §4.1 N=1 supervisor).
    Additive column `ff_exec_core.lease_reclaim_count INTEGER NOT NULL
    DEFAULT 0`. No JSON backfill: SQLite never shipped the PG scheduler
    `claim_grant` JSON stash (RFC-023 §5 `claim_for_worker` non-goal).
  - `crates/ff-backend-sqlite/src/queries/claim_grant.rs` — new
    dialect-forked SQL strings (INSERT / SELECT / DELETE / UPDATE)
    mirroring the PG reference one-for-one.
  - `crates/ff-backend-sqlite/src/reclaim.rs` — new module carrying
    `issue_reclaim_grant_impl` + `reclaim_execution_impl`. Both run
    inside a single `BEGIN IMMEDIATE` txn (RFC-023 §4.3 RESERVED
    write-lock substitute for PG `FOR UPDATE`). `reclaim_execution_impl`
    marks the prior attempt `interrupted_reclaimed`, inserts a fresh
    `ff_attempt` row (new attempt_index), bumps `lease_reclaim_count`,
    consumes the grant row, emits an RFC-019 `reclaimed` lease event
    via the post-commit `PubSub::lease_history` broadcast, and mints
    a `HandleKind::Reclaimed` handle. Cap exceeded branch transitions
    the execution to `terminal_failed` matching the Valkey Lua at
    `flowfabric.lua:3049-3080`.
  - `crates/ff-backend-sqlite/src/backend.rs` — `EngineBackend::issue_reclaim_grant`
    + `EngineBackend::reclaim_execution` trait methods overridden
    (wrapped in `retry_serializable`).
  - `crates/ff-backend-sqlite/tests/rfc024_reclaim.rs` — 10 new
    integration tests covering happy path, wrong-phase rejection,
    `lease_revoked` admission, new-attempt semantics,
    prior-attempt-interrupted marker, worker-id mismatch,
    grant-not-found, grant-TTL-expired, cap-exceeded terminal
    transition, and `HandleKind::Reclaimed` handle kind.
  - Default max_reclaim_count: 1000 on the Rust surface (per RFC-024
    §4.6); per-call override via `ReclaimExecutionArgs.max_reclaim_count`.
- **RFC-024 PR-B+C — trait method renames + new `ReclaimGrant` type +
  non_exhaustive constructors (closes partial of #371).** Folded PR-B
  and PR-C of the RFC-024 series because the new trait method
  signatures reference types introduced in PR-C; shipping them
  separately would land trait methods referencing undefined types.
  Scope spans `ff-core`, all three backends (Valkey, Postgres,
  SQLite), `ff-sdk`, `ff-scheduler`, `ff-server`, and `ff-test`.
  Backend impl bodies for the new methods are still
  `EngineError::Unavailable` — PR-D (PG), PR-E (SQLite), PR-F
  (Valkey) wire the real bodies.
  - `crates/ff-core/src/contracts/mod.rs` — new distinct `ReclaimGrant`
    type (lease-reclaim path, routes to `ff_reclaim_execution` /
    PG+SQLite `reclaim_execution`). Distinct from the PR-A-renamed
    `ResumeGrant`. `#[non_exhaustive]` with explicit `::new()`
    constructor per `feedback_non_exhaustive_needs_constructor`.
  - `crates/ff-core/src/contracts/mod.rs` — new
    `IssueReclaimGrantOutcome` enum (`Granted(ReclaimGrant)` /
    `NotReclaimable` / `ReclaimCapExceeded`) and new
    `ReclaimExecutionOutcome` enum (`Claimed(Handle)` /
    `NotReclaimable` / `ReclaimCapExceeded` / `GrantNotFound`). Both
    `#[non_exhaustive]`; variants ARE the construction surface.
  - `crates/ff-core/src/engine_backend.rs` — two new trait methods:
    `issue_reclaim_grant(args) -> IssueReclaimGrantOutcome` and
    `reclaim_execution(args) -> ReclaimExecutionOutcome`. Default
    impls return `EngineError::Unavailable` so pre-RFC out-of-tree
    backends keep compiling.

### Changed

- **RFC-024 PR-F — `Supports::issue_reclaim_grant` capability flag**
  added to `ff_core::capability::Supports` (RFC-018 surface). One bool
  covers both `issue_reclaim_grant` + `reclaim_execution` per
  RFC-024 §3.6 (the two trait methods always land together on every
  in-tree backend, so rolling them up matches the group-level policy
  on `budget_admin` / `quota_admin`). Defaults to `false` via
  `Supports::none()`; Valkey's `valkey_supports_base()` flips it to
  `true` in this PR.
- **RFC-024 PR-D — Postgres scheduler claim-grant storage flips
  from JSON stash to `ff_claim_grant` table.** Pre-PR-D the
  scheduler wrote claim grants into
  `ff_exec_core.raw_fields.claim_grant` as a JSON object (see
  `scheduler.rs:252` pre-PR-D). Post-PR-D, the scheduler writes a
  `kind='claim'` row to the new `ff_claim_grant` table; the
  `verify_grant` read path reads worker identity from the same
  table. `raw_fields.claim_grant` is left in place for one
  release for backfill safety.

- **RFC-024 PR-B+C — trait method rename + transitional aliases
  dropped.** Compile-break wave that lands in the same PR as the new
  surface above.
  - `EngineBackend::claim_from_reclaim` → `claim_from_resume_grant`
    across all three in-tree backends (Valkey, Postgres, SQLite) and
    the SDK layer forwarders (`ff-sdk/src/layer/hooks.rs`,
    `layer/test_support.rs`). Body unchanged; the method has always
    driven `ff_claim_resumed_execution` / the PG+SQLite epoch-bump
    reconciler — the old name advertised "reclaim" but delivered
    resume.
  - `FlowFabricWorker::claim_from_reclaim_grant` →
    `claim_from_resume_grant` on the ff-sdk consumer surface. The
    method retains its semantic (feeds the resume-grant path); the
    new `claim_from_reclaim_grant` SDK method (distinct semantic,
    feeds `reclaim_execution`) lands with PR-G.
  - `ClaimGrant`, `ResumeGrant`, `IssueReclaimGrantArgs`, and
    `ReclaimExecutionArgs` gain `#[non_exhaustive]` + explicit
    `::new()` constructors per
    `feedback_non_exhaustive_needs_constructor`. Struct-literal
    construction from downstream crates migrates to the
    constructors; in-crate tests unaffected.
  - `IssueReclaimGrantArgs` gains a `worker_capabilities:
    BTreeSet<String>` field (parity with `IssueClaimGrantArgs`; the
    Lua `ff_issue_reclaim_grant` already reads ARGV[9]). Populated
    by the SDK admin path in PR-G; `#[serde(default)]` preserves
    wire compat.
  - `ReclaimExecutionArgs::max_reclaim_count` flips from `u32` to
    `Option<u32>` per RFC-024 §3.2. `None` ⇒ backend applies the
    Rust-surface default of 1000 (RFC-024 §4.6); explicit `Some(n)`
    preserves the per-call override. The Lua fallback remains 100
    for pre-RFC ARGV-omitted callers; the two-default coexistence
    is explicit by design.
  - `crates/ff-core/src/contracts/mod.rs` — PR-A's transitional
    `pub type ReclaimGrant = ResumeGrant` alias dropped. Call sites
    semantically using a resume grant migrate to `ResumeGrant`; the
    (distinct) new `ReclaimGrant` type in the `### Added` section
    above covers the lease-reclaim path.
  - `crates/ff-core/src/backend.rs` — PR-A's transitional
    `pub type ReclaimToken = ResumeToken` alias dropped; all call
    sites migrate to `ResumeToken`.

- **RFC-024 PR-A — `ff-core` resume-path rename + `HandleKind::Reclaimed` variant.**
  First impl PR of the RFC-024 series (the series as a whole wires
  `ff_reclaim_execution` + `ff_issue_reclaim_grant` to the consumer
  surface; this PR ships only the type renames + additive
  `HandleKind::Reclaimed` variant — no new trait methods, no backend
  wiring, no SDK endpoint). Scope is ff-core-only; no backend / SDK
  / server code touched.
  - `crates/ff-core/src/contracts/mod.rs` — `ReclaimGrant` struct
    renamed to `ResumeGrant` (the semantic has always been resume-
    after-suspend; the routing FCALL is `ff_claim_resumed_execution`).
    A transitional `pub type ReclaimGrant = ResumeGrant` alias is
    retained so downstream backend / SDK / scheduler crates keep
    compiling until the follow-up PR rewrites their call sites.
  - `crates/ff-core/src/backend.rs` — `ReclaimToken` struct renamed
    to `ResumeToken` (wraps a `ResumeGrant`; feeds the resume path).
    Transitional `pub type ReclaimToken = ResumeToken` alias
    retained on the same rationale.
  - `crates/ff-core/src/backend.rs` — new `HandleKind::Reclaimed`
    variant on the `#[non_exhaustive]` enum. Purely additive.
    Reserved for the forthcoming `reclaim_execution` trait method
    (RFC-024 PR-C onward) that mints a handle via the new
    lease-reclaim path (distinct from the existing `Resumed`
    suspend/resume path).
  - Internal ff-core docs + the single `claim_from_reclaim` trait
    method signature use the new names directly; no `#[allow(deprecated)]`
    scatter. The aliases carry no `#[deprecated]` attribute (workspace
    clippy runs with `-D warnings` and the downstream rename sweep
    ships separately).
  - Defers to the next PR in the series: the new `ReclaimGrant`
    type (distinct semantic, lease-reclaim path), `#[non_exhaustive]` +
    `new()` constructors on `ClaimGrant` / `ResumeGrant` /
    `IssueReclaimGrantArgs` / `ReclaimExecutionArgs`,
    `max_reclaim_count: u32` → `Option<u32>` flip, and the new
    `IssueReclaimGrantOutcome` + `ReclaimExecutionOutcome` enums.
    Those ride with the new trait-method introduction (PR-C) so the
    constructor stabilisation lands alongside the surface that
    requires it.

- **`crates/ff-backend-sqlite` — Phase 3.5 scanner supervisor (N=1) +
  `budget_reset` reconciler (RFC-023 Phase 3.5 / RFC-020 Wave 9
  Standalone-1).** Ports the Postgres `scanner_supervisor` +
  `reconcilers::budget_reset` to SQLite, collapsed to N=1 per §4.1
  (SQLite is single-writer / single-process → no partition
  fan-out). Supervisor = one `tokio::time::interval` tick task per
  reconciler with a `watch` shutdown channel; drained on
  `EngineBackend::shutdown_prepare`. `ff-server::start_sqlite_branch`
  installs the supervisor using the same `budget_reset_interval`
  env knob that drives the Valkey + Postgres backends.
- `crates/ff-backend-sqlite/src/scanner_supervisor.rs` — supervisor
  handle + tick loop + `shutdown(grace)` API.
- `crates/ff-backend-sqlite/src/reconcilers/{mod,budget_reset}.rs` —
  `budget_reset` reconciler body (partial-index-backed bounded batch
  scan at `BATCH_SIZE = 20`; delegates per-row to the shared
  `budget::budget_reset_reconciler_apply` helper).
- `SqliteBackend::with_scanners(cfg) -> bool` — idempotent
  supervisor install; `shutdown_prepare(grace)` override to drain.

- **`crates/ff-backend-sqlite` — Phase 3.4 Wave 9 budget/quota admin
  (RFC-023 Phase 3.4 / RFC-020 §4.4 Rev 6).** Replaces 5
  `Unavailable` trait defaults with real SQLite bodies ported from
  `ff-backend-postgres/src/budget.rs`: `create_budget` (INSERT
  `ff_budget_policy` with 8 Rev-6 columns + idempotent
  `ON CONFLICT DO NOTHING`), `reset_budget` (zero `ff_budget_usage` +
  clear breach metadata + advance `next_reset_at_ms`),
  `create_quota_policy` (INSERT `ff_quota_policy`;
  `ff_quota_window` + `ff_quota_admitted` tables from migration 0012
  exist and are written by the future admission path, matching PG's
  `create_quota_policy_impl`), `get_budget_status` (two SELECTs —
  policy row + usage rows), `report_usage_admin` (admin-path
  shared-core entry; translates the parallel dim/delta vectors to
  `UsageDimensions`).
- `crates/ff-backend-sqlite/src/budget.rs` — new module hosting the
  5 method bodies + `report_usage_and_check_core` shared hot-path.
- `crates/ff-backend-sqlite/src/queries/budget.rs` +
  `queries/quota.rs` — SQLite-dialect SQL constants (positional `?`
  placeholders; shape-identical to the PG reference).

### Changed

- **`benches/comparisons/apalis` — scenario 1 + 4 harnesses refreshed
  per apalis maintainer @geofmureithi's tuning recommendations on
  issue #51.** `RedisConfig` now sets `poll_interval=5ms` +
  `buffer_size=100` (was default 100ms×10 = ~100 ops/s theoretical
  cap). The scenarios spawn N=16 independent `WorkerBuilder`
  instances with `.parallelize(tokio::spawn)` rather than a single
  builder with `.concurrency(16)` — per the maintainer,
  `.concurrency` governs in-worker task parallelism, not worker
  count. Refreshed numbers: scenario 1 goes 98 → 1 419 ops/s
  (14.5×), scenario 4 goes 9.34 → 10.2 flows/s. `SharedRedisStorage`
  pubsub declined (drops on failure; benches measure durable
  at-least-once). See `benches/results/COMPARISON.md`.

- `ff-backend-sqlite::report_usage` — extended to maintain the 0013
  breach-counter columns (`breach_count`, `soft_breach_count`,
  `last_breach_at_ms`, `last_breach_dim`) incrementally in the same
  write tx as the `ff_budget_usage` increments, matching Valkey
  parity at `flowfabric.lua:6576-6580,6614` and the PG Rev-6 §7.2
  pin-lift. Single-writer `BEGIN IMMEDIATE` + `retry_serializable`
  replaces PG's `FOR NO KEY UPDATE` lock discipline per RFC-023
  §4.3.

- **`crates/ff-backend-sqlite` — Phase 3.3 Wave 9 read model +
  cancel-flow split + waitpoint read (RFC-023 Phase 3.3 / RFC-020 §4.1
  + §4.2.3 + §4.5).** Replaces 6 `Unavailable` trait defaults with
  real SQLite bodies ported from the Postgres reference
  (`ff-backend-postgres/src/exec_core.rs` + `operator.rs` +
  `suspend_ops.rs`). Read model: `read_execution_state` (single-column
  projection of `public_state`), `read_execution_info` (multi-column
  projection + correlated attempt subqueries → `ExecutionInfo`), and
  `get_execution_result` (current-attempt semantics per Rev 7 Fork 3).
  Cancel-flow split: `cancel_flow_header` (atomic flip of flow_core +
  backlog header + member rows + per-member exec_core flip + outbox
  `flow_cancel_requested`; idempotent replay returns
  `AlreadyTerminal { stored_* }`), and `ack_cancel_member` (member
  drain + conditional parent delete; NO outbox emit per §4.2.7
  Valkey-parity). Waitpoint read: `list_pending_waitpoints`
  (cursor-paginated scan of `ff_waitpoint_pending` with `state IN
  ('pending','active')` filter, `NotFound` on missing execution,
  `(token_kid, token_fingerprint)` parsed from the stored token).
  Storage-tier literal normalisation reuses the PG helper shapes:
  `cancelled`/`terminal` → Terminal; `running` → Active; `pending`/
  `pending_claim` → PendingFirstAttempt; unknown tokens surface as
  `EngineError::Validation(Corruption)` via `json_enum!`.
- `crates/ff-backend-sqlite/src/reads.rs` — new module hosting the 3
  read bodies + 5 `normalise_*` helpers + `derive_terminal_outcome`.
- `crates/ff-backend-sqlite/src/queries/reads.rs` — SQL for the 3 read
  paths. SQLite `LEFT JOIN LATERAL` lowered to correlated subqueries
  in the SELECT list.
- `crates/ff-backend-sqlite/src/operator.rs` — extended with
  `cancel_flow_header_impl` + `ack_cancel_member_impl` + helper
  `insert_operator_event_flow` (back-fills namespace from
  `ff_flow_core.raw_fields`).
- `crates/ff-backend-sqlite/src/queries/operator.rs` — extended with
  cancel-flow-header SQL set (`SELECT_FLOW_CORE_FOR_CANCEL_SQL`,
  `UPDATE_FLOW_CORE_CANCEL_WITH_REASON_SQL`, `INSERT_CANCEL_BACKLOG_SQL`,
  `SELECT_FLOW_INFLIGHT_MEMBERS_SQL`, `INSERT_CANCEL_BACKLOG_MEMBER_SQL`,
  `UPDATE_EXEC_CORE_CANCEL_FROM_HEADER_SQL`,
  `DELETE_CANCEL_BACKLOG_MEMBER_SQL`,
  `DELETE_CANCEL_BACKLOG_IF_EMPTY_SQL`) plus
  `INSERT_OPERATOR_EVENT_FLOW_SQL` for flow-keyed operator events.
- `crates/ff-backend-sqlite/src/queries/waitpoint.rs` — extended with
  `SELECT_EXEC_EXISTS_SQL` + `SELECT_PENDING_WAITPOINTS_PAGE_SQL` for
  the paginated pending-waitpoint scan.
- `crates/ff-backend-sqlite/src/suspend_ops.rs` — extended with
  `list_pending_waitpoints_impl` + `parse_waitpoint_token_kid_fp`
  (mirrors the PG reference shape).
- `crates/ff-backend-sqlite/tests/wave9_reads.rs` — 9 integration
  tests covering the read model (missing → None, post-create,
  post-claim, normalisation, terminal-outcome derivation, active/
  terminal result semantics).
- `crates/ff-backend-sqlite/tests/wave9_waitpoints.rs` — 5 integration
  tests covering `list_pending_waitpoints` (empty, happy path with all
  10 `PendingWaitpointInfo` fields populated, cursor pagination across
  3 pages, `NotFound` on missing execution, `state='closed'` filter).
- `crates/ff-backend-sqlite/tests/wave9_operator.rs` — extended with
  4 integration tests for `cancel_flow_header` (happy path with
  backlog + member + operator_event + exec_core verification,
  idempotent `AlreadyTerminal` replay) and `ack_cancel_member`
  (member-drain + final-ack parent-delete, idempotent no-op on missing).

- **`crates/ff-backend-sqlite` — Phase 3.2 Wave 9 operator control
  (RFC-023 Phase 3.2 / RFC-020 §4.2.1-5).** Replaces the 4 `Unavailable`
  trait defaults for `cancel_execution`, `revoke_lease`,
  `change_priority`, and `replay_execution` with real SQLite bodies
  ported from the Postgres reference (`ff-backend-postgres/src/operator.rs`
  v0.11.0). Each method runs under the §4.2 shared spine adapted for
  SQLite (`BEGIN IMMEDIATE` RESERVED-lock for the RMW window, WHERE-
  clause CAS fencing, `rows_affected()`-driven contention branches,
  outbox INSERT in-tx + post-commit broadcast wakeup) and rides the
  Phase 2a.1 `retry::retry_serializable` wrapper for SQLITE_BUSY /
  SQLITE_LOCKED absorption. Semantics match PG Revision 7 exactly:
  `cancel_execution` emits `ff_lease_event revoked` only (no
  operator_event — the migration 0010 CHECK allow-list gates on
  `priority_changed` / `replayed` / `flow_cancel_requested`);
  `revoke_lease` returns `AlreadySatisfied` on `no_active_lease` /
  `different_worker_instance_id` / `lease_id_mismatch` / `epoch_moved`;
  `change_priority` surfaces `Contention(ExecutionNotEligible)` on the
  Valkey-canonical gate (`lifecycle_phase='runnable' AND
  eligibility_state='eligible_now'`) failing; `replay_execution` picks
  the skipped-flow-member branch iff the current attempt's
  `outcome='skipped' AND exec_core.flow_id IS NOT NULL` (Valkey
  `flowfabric.lua:8555`) and resets downstream `ff_edge_group`
  skip/fail/running counters to 0 while preserving `success_count` per
  Revision 7 Option A. Replay bumps `raw_fields.replay_count` via a
  nested `json_set(json_set(...))` (SQLite JSON1 analogue of PG's
  `jsonb_set(..., to_jsonb(... + 1))`).
- `crates/ff-backend-sqlite/src/operator.rs` — 4 `_once` bodies + 4
  `_impl` retry wrappers + in-file helpers (`split_eid`,
  `synthetic_lease_id`, `insert_lease_event`, `insert_operator_event`,
  post-commit broadcast dispatchers).
- `crates/ff-backend-sqlite/src/queries/operator.rs` — dialect-forked
  SQL for the 4 methods (pre-reads, CAS UPDATEs, edge-group reset,
  co-transactional `INSERT_OPERATOR_EVENT_SQL` that back-fills
  `namespace` + `instance_tag` from `ff_exec_core.raw_fields`).
- `crates/ff-backend-sqlite/tests/wave9_operator.rs` — 13-test
  integration harness: happy-path + fence-failure + gate-failure +
  outbox-verify for each method, the two replay branches (normal
  resumes to `Waiting`, skipped-flow-member resumes to
  `WaitingChildren` and resets edge-group counters with
  `success_count` preserved), and a concurrent-cancel serialization
  test that verifies SERIALIZABLE-retry absorbs cross-tx SQLITE_BUSY
  under the single-writer envelope.
- **`crates/ff-backend-sqlite` — Phase 3.1 subscribe methods (RFC-023
  Wave 9 / RFC-019 Stage B/C).** Replaces the 3 `Unavailable` defaults
  for `subscribe_completion`, `subscribe_lease_history`, and
  `subscribe_signal_delivery` with real bodies built on the Phase
  2b.2.2 `OutboxCursorReader` primitive: catch-up SELECT on subscribe,
  park on the matching `pubsub` broadcast channel for post-commit
  wakeups, re-SELECT on each wake, decode typed events via per-family
  closures. Cursor encoding reuses the Postgres family prefix
  (`0x02 ++ event_id(BE8)`) — SQLite's i64 event_ids are equivalent
  in shape — so cursors are wire-stable across backends. `ScannerFilter`
  is applied in the row decoder against the outbox's denormalised
  `namespace` + `instance_tag` columns (matching the PG reference at
  `ff-backend-postgres/src/lease_event_subscribe.rs`); NULL filter
  columns silently drop under any non-noop filter, preserving the
  cross-backend "filtered subscribers drop NULL-column rows"
  invariant. `subscribe_instance_tags` remains on the trait default
  (`Unavailable`) per RFC-020 §3.2 / the #311 deferral.
- `crates/ff-backend-sqlite/src/completion_subscribe.rs`,
  `src/lease_event_subscribe.rs`, `src/signal_delivery_subscribe.rs`
  — one module per subscribe method; each owns its SELECT SQL, row
  decoder, and typed-event mapping. Module naming mirrors the PG
  reference for straight cross-backend diff.
- `crates/ff-backend-sqlite/tests/subscribe.rs` — 10-test integration
  harness covering tail happy-path, cursor-resume across subscribe
  sessions, `ScannerFilter::instance_tag` behaviour (positive match
  through the signal-delivery outbox where the producer populates
  the column via `json_extract`; null-drop on lease/completion where
  the producer writes NULL today), and lagged-broadcast recovery
  (300 completions vs the 256-slot broadcast ring, verifying the
  cursor-select fallback catches every durable row after a
  `RecvError::Lagged`).
- **`crates/ff-backend-sqlite` — Phase 2b.2.2 stream readers + outbox
  cursor-resume primitive (RFC-023 Group C + Group D.2; completes
  Phase 2).** Replaces the 3 Group C `Unavailable` stubs with real
  bodies: `read_stream` (XRANGE-equivalent over `(ts_ms, seq)`),
  `tail_stream` (opportunistic SELECT + `stream_frame` broadcast park
  + re-SELECT, `TailVisibility::ExcludeBestEffort` filters
  `mode <> 'best_effort'` in SQL), and `read_summary` (row fetch from
  `ff_stream_summary` with byte-normalized `document_json`). Every
  body mirrors `ff-backend-postgres/src/stream.rs`; the broadcast
  park replaces PG's `LISTEN/NOTIFY` (RFC-023 §4.2) and no pool
  connection is held while parked.
- `crates/ff-backend-sqlite/src/outbox_cursor.rs` — generic
  cursor-resume + broadcast-wakeup reader primitive. One
  `OutboxCursorConfig<T>` takes pool, per-outbox SELECT SQL,
  partition key, cursor watermark, batch size, broadcast receiver,
  row decoder, and event-id extractor; `spawn()` returns an
  `OutboxCursorStream<T>` adapter over `futures_core::Stream`.
  Handles the three steady-state paths that Phase 3 `subscribe_*`
  impls need: (1) cursor-resume catch-up before attaching
  broadcast, (2) `RecvError::Lagged(n)` → fall back to cursor-select
  (outbox is durable so lagged wakeups are harmless), (3) clean
  shutdown when the consumer drops the stream or the backend drops
  the broadcast sender. Closure-based decoder keeps the primitive
  column-agnostic so Phase 3's 5 outbox tables each plug in their
  own row-typing without a shared `OutboxRow` trait zoo. Exercised
  end-to-end by 3 unit tests against a real `stream_frame`
  producer (`append_frame` → broadcast → reader yields typed
  event): catch-up-then-live, cursor-resume skips seen rows, clean
  shutdown on stream drop.
- `crates/ff-backend-sqlite/src/queries/stream.rs` — read-side SQL
  constants: `READ_STREAM_RANGE_SQL` (inclusive `(ts_ms, seq)`
  range), `TAIL_STREAM_AFTER_SQL` + `TAIL_STREAM_AFTER_EXCLUDE_BE_SQL`
  (strictly after cursor, with/without best-effort filter),
  `READ_SUMMARY_FULL_SQL` (five-column summary fetch),
  `OUTBOX_TAIL_STREAM_FRAME_SQL` (ROWID-based outbox tail).
- `crates/ff-backend-sqlite/tests/stream_reader.rs` — 10-test
  integration harness covering happy-path `read_stream`, cursor
  pagination, per-attempt-index isolation, summary merge/missing,
  `tail_stream` fast-path / wake-on-append / timeout / open-cursor
  rejection / best-effort visibility filter. Seeds via the Phase 2a
  producer path (`claim` + `append_frame`) so the tests double as
  end-to-end coverage from the producer outbox emit through to the
  reader surface.

- **`crates/ff-backend-sqlite` — Phase 2b.2.1 suspend/signal/waitpoint
  producer (RFC-023 Group B, minus `list_pending_waitpoints` deferred
  to Phase 3).** Replaces 6 Group B `Unavailable` stubs with real
  bodies + adds 2 overrides for the default-`Unavailable` HMAC secret
  methods: `suspend`, `suspend_by_triple`, `create_waitpoint`,
  `observe_signals`, `deliver_signal`, `claim_resumed_execution`,
  `seed_waitpoint_hmac_secret`, `rotate_waitpoint_hmac_secret_all`.
  Every body mirrors `ff-backend-postgres/src/suspend_ops.rs` +
  `ff-backend-postgres/src/signal.rs` statement-by-statement; SQLite
  dialect changes are `jsonb → TEXT` JSON codec (serde_json at the
  Rust boundary), `text[] → TEXT` holding a JSON array literal for
  `required_signal_names`, and `BEGIN IMMEDIATE` single-writer
  serialization replacing PG SERIALIZABLE + `FOR UPDATE`. The
  composite-condition evaluator is a pure-Rust copy of the PG reference
  (`ff-backend-postgres/src/suspend.rs::evaluate`) kept local to the
  SQLite module tree to avoid a cross-backend crate dependency. Signal
  delivery emits a post-commit wakeup on the `signal_delivery`
  broadcast channel; a signal that satisfies the resume condition also
  writes a completion outbox row and wakes `completion`, and
  `claim_resumed_execution` writes an `acquired` lease-event row + wakes
  `lease_history`. Dedup cache (`ff_suspend_dedup`) honours the
  RFC-013 §3 replay ladder so an idempotent retry returns the same
  `suspension_id` + token bytes as the original call.
- `crates/ff-backend-sqlite/src/queries/{suspend,signal,waitpoint}.rs`
  — SQLite SQL constants mirroring the PG bodies; no bodies live in
  these modules, only the statement strings used by `suspend_ops.rs`.
- `crates/ff-backend-sqlite/src/suspend_ops.rs` — the Group B bodies,
  paralleling `ff-backend-postgres/src/suspend_ops.rs`.
- `crates/ff-backend-sqlite/tests/producer_suspend.rs` — 8-test
  integration harness covering happy-path suspend, idempotent replay,
  fence-mismatch contention, HMAC-seeded create_waitpoint token
  format, deliver_signal happy-path + append-without-satisfaction, the
  satisfy→claim_resumed round-trip, seed idempotency, and rotate
  replay semantics.
- **`crates/ff-core::crypto::hmac` — extracted HMAC-SHA256 sign/verify
  primitives.** The Postgres backend's in-crate `hmac_sign` /
  `hmac_verify` / `HmacVerifyError` move to `ff_core::crypto::hmac`
  with zero behaviour change so both Postgres + SQLite consume a
  single Rust implementation (Valkey continues to sign inside Lua).
  The PG `signal.rs` re-exports from ff-core for backward
  compatibility of internal call sites.

### Fixed

- **ff-backend-sqlite: claim path writes `public_state = 'running'` to
  `ff_exec_core`.** Matches the Postgres parity write at
  `ff-backend-postgres/src/suspend_ops.rs:958-960`. Before the fix the
  column stayed at its create-time `'waiting'` literal on claimed
  executions (`'pending'` is the sibling `attempt_state` literal, not
  `public_state`); the Spine-B read normaliser inferred the correct
  `PublicState::Active` from lifecycle + ownership so downstream
  consumers were unaffected, but direct SQL reads against
  `ff_exec_core.public_state` observed the wrong state.
- **ff-backend-sqlite: `SqliteBackend::new` `is_memory` detection now
  catches `file:*?mode=memory*` URIs (closes #372).** The prior
  detector (`path == ":memory:" || path.starts_with("file::memory:")`)
  missed the RFC-023 §4.6 recommended test-isolation form
  (`file:ff-test-<uuid>?mode=memory&cache=shared`), so WAL mode was
  applied inappropriately and no sentinel connection was held — a
  pool-idle cycle dropped the shared cache mid-test and risked data
  loss under parallel test harnesses. Detection is extracted into a
  dedicated `is_memory_uri` helper with a unit test covering all three
  supported forms.
- **ferriskey: expose `Client::force_cluster_slot_refresh` (closes
  #369).** New `pub async fn force_cluster_slot_refresh(&self) ->
  Result<()>` on `ferriskey::Client` (and on the internal
  `ClientInner` + `cluster::ClusterConnection::force_slot_refresh`)
  wraps the existing internal
  `refresh_slots_and_subscriptions_with_retries` with
  `RefreshPolicy::NotThrottable` +
  `SlotRefreshTrigger::RuntimeRefresh`. Standalone mode is a no-op
  returning `Ok(())` without a network round-trip. Intended for
  consumers that have observed a stale-topology symptom (e.g.
  `READONLY` on a write after failover) and want the next retry to
  use a fresh slot map.
- **`ff-script::loader` — force a cluster slot-map refresh between
  `FUNCTION LOAD` retries.** The old retry loop slept blindly (1s /
  2s / 4s / 8s … = ~39s total) without nudging ferriskey's slot
  cache, so every attempt re-used the same stale routing and the
  `READONLY` surface re-occurred until the background refresher
  happened to fire. Each retry now calls
  `client.force_cluster_slot_refresh().await` before the sleep,
  making attempts productive. First attempt still skips the refresh
  so the happy-path cluster has no extra RTT.
- **`ff-script::loader` — shorten retry window from ~39s to ~15s
  (6 attempts with 500ms/1s/2s/4s/7s backoff).** Retries are
  productive now (each actually refreshes topology), so the blind
  defense-in-depth padding is no longer needed.

- **`crates/ff-backend-sqlite` — outbox producer tag back-fill
  (Phase 3.1-flagged regression, fixed in Phase 3.2).** Prior to
  Phase 3.2 the SQLite `ff_lease_event` + `ff_completion_event`
  producers hard-coded `namespace` + `instance_tag` to `NULL` on every
  insert, causing any `ScannerFilter::instance_tag(...)` subscriber
  to silently drop every lease + completion event (the
  `subscribe_*_filter_drops_null_tag_rows` tests pinned this shape;
  the signal producer had already been ported correctly in Phase
  2b.2.1). Phase 3.2 replaces the hard-coded NULLs with a
  co-transactional `SELECT ... FROM ff_exec_core UNION ALL SELECT ...
  WHERE NOT EXISTS (...)` (mirror of
  `queries/signal.rs::INSERT_SIGNAL_EVENT_SQL`) so every outbox row
  carries the denormalised tag columns. Touched producers:
  `queries/dispatch.rs::INSERT_LEASE_EVENT_SQL`,
  `queries/attempt.rs::INSERT_COMPLETION_EVENT_SQL`,
  `queries/signal.rs::INSERT_COMPLETION_RESUMABLE_SQL`. The 3
  call-sites on `INSERT_LEASE_EVENT_SQL`
  (`backend::insert_lease_event`,
  `suspend_ops::claim_resumed_execution_impl`, new
  `operator::insert_lease_event`) now bind the exec UUID twice: once
  stringified for the outbox row and once as the 16-byte BLOB for the
  exec_core lookup. The two previously-xfailing subscribe tests
  (`subscribe_*_filter_drops_null_tag_rows`) flip to positive
  filter-through
  (`subscribe_*_filter_by_instance_tag_receives_events`).

### Changed

- **`crates/ff-backend-postgres` — HMAC primitives hoisted to
  `ff-core`.** `ff-backend-postgres/src/signal.rs::{hmac_sign,
  hmac_verify, HmacVerifyError}` are now re-exports from
  `ff_core::crypto::hmac`. No consumer-visible change: the wire shape
  (`<kid>:<hex>`) and the `HmacVerifyError` variants are byte- and
  variant-identical. The `hmac` + `sha2` direct dependencies are
  dropped from `ff-backend-postgres`' `Cargo.toml` (now transitive
  through `ff-core`); `hex` stays — `suspend_ops.rs` still uses it for
  opaque-handle + payload hex codec.

- **`crates/ff-backend-sqlite` — Phase 2b.1 producer exec/flow + pubsub
  foundation (RFC-023 Group A + D.1).** Replaces 6 `Unavailable` stubs
  with real bodies: `create_execution` (seeds `ff_exec_core` + the
  `ff_execution_capabilities` junction + `ff_lane_registry`),
  `create_flow` (idempotent `ff_flow_core` insert), `add_execution_to_flow`
  (stamps `exec_core.flow_id` + bumps `graph_revision` /
  `raw_fields.node_count`), `stage_dependency_edge` (CAS on
  `graph_revision`, inserts into `ff_edge`), `apply_dependency_to_child`
  (marks edge applied, upserts `ff_edge_group.running_count`), and
  classic `cancel_flow` (flips flow header + member `ff_exec_core`
  rows, writes completion + lease-revoked outbox rows per member,
  queues `ff_pending_cancel_groups` under `CancelPending`). Every
  body mirrors `ff-backend-postgres/src/flow{,_staging}.rs` +
  `ff-backend-postgres/src/exec_core.rs::create_execution_impl`
  statement-by-statement; SQLite dialect changes are contained to
  `json_set` + `json_extract` (JSON1) replacing PG `jsonb_set` /
  `jsonb_build_object`, and `BEGIN IMMEDIATE` single-writer
  serialization replacing PG `FOR UPDATE` row locks. The `wait`
  axis on `cancel_flow` always reports synchronous `Cancelled {..}`
  — under single-writer SQLite every member flip commits in the
  header transaction. Group B (suspend/signal/deliver_signal),
  Group C (stream reads), and Group D.2 (subscribe trait surface)
  land in Phase 2b.2.
- **`crates/ff-backend-sqlite/src/pubsub.rs` — full post-commit
  broadcast topology.** Five `tokio::sync::broadcast` channels,
  one per outbox table (`lease_history`, `completion`,
  `signal_delivery`, `stream_frame`, `operator_event`); the
  `OutboxEvent` payload carries `(event_id, partition_key)` read
  from `last_insert_rowid()` inside the writing transaction, with
  `dispatch_pending_emits` firing AFTER `tx.commit()` returns
  (RFC-023 §4.2 A2 — broadcast is wakeup only, durable replay
  rides `event_id > cursor` on the outbox tables). Post-commit
  emit wiring added to the 7 existing Phase 2a methods that
  write outbox rows: `claim` / `complete` / `fail` / `renew` /
  `append_frame` / `claim_from_reclaim`, plus the new
  `cancel_flow` path's per-member completion + lease-revoked
  emits.
- `crates/ff-backend-sqlite/src/queries/flow.rs` (new) +
  `crates/ff-backend-sqlite/src/queries/flow_staging.rs` (new)
  carrying the SQLite-dialect SQL const block for the Group A
  surface; `queries/exec_core.rs` extended with `INSERT_EXEC_CORE_SQL`
  + `INSERT_EXEC_CAPABILITY_SQL` + `INSERT_LANE_REGISTRY_SQL`;
  `queries/dispatch.rs` populated with `INSERT_LEASE_EVENT_SQL` +
  scaffolding consts for `INSERT_SIGNAL_EVENT_SQL` /
  `INSERT_OPERATOR_EVENT_SQL` (Phase 2b.2 consumers).
- `crates/ff-backend-sqlite/tests/producer_flow.rs` (new) covering
  `create_execution` + junction populate, idempotent `create_flow`,
  3-member flow build + 2-edge stage + apply + `cancel_flow`
  member fan-out, `stage_dependency_edge` stale-revision
  rejection, and a `complete()` → outbox-row-present smoke that
  exercises the pubsub post-commit path end-to-end.
- **`crates/ff-backend-sqlite` — Phase 2a.3 lease-lifecycle + progress +
  append_frame (RFC-023).** Replaces the `Unavailable` stubs for `renew`,
  `progress`, `append_frame`, and `claim_from_reclaim` with real bodies
  paralleling `ff-backend-postgres::attempt` + `ff-backend-postgres::stream`.
  Fence CAS via `SELECT_ATTEMPT_EPOCH_SQL` under `BEGIN IMMEDIATE`
  (§4.1 A3) gates every write; `LeaseConflict` on mismatch, no retry.
  Lease-event outbox emits `renewed` / `reclaimed` rows to
  `ff_lease_event` for RFC-019 subscribe parity. `append_frame` covers
  the full RFC-015 write surface — `Durable`, `DurableSummary` with
  in-Rust JSON Merge Patch (RFC 7396 incl. `__ff_null__` sentinel), and
  `BestEffortLive` with EMA-driven MAXLEN trim. Read surface
  (`read_stream` / `tail_stream` / `read_summary`) remains deferred
  (Phase 2b). `progress` merges `progress_pct` + `progress_message`
  into `ff_exec_core.raw_fields` via `json_set(coalesce(...))` so
  partial updates preserve prior values.
- `crates/ff-backend-sqlite/src/queries/lease.rs` populated with
  `UPDATE_ATTEMPT_RENEW_SQL`, `SELECT_LATEST_ATTEMPT_FOR_RECLAIM_SQL`,
  `UPDATE_ATTEMPT_RECLAIM_SQL`, `UPDATE_EXEC_CORE_RECLAIM_SQL`.
- `crates/ff-backend-sqlite/src/queries/stream.rs` (new) populated with
  the RFC-015 write-path SQL: `SELECT_MAX_SEQ_SQL`,
  `INSERT_STREAM_FRAME_SQL`, `SELECT_STREAM_SUMMARY_SQL`,
  `INSERT_STREAM_SUMMARY_SQL`, `UPDATE_STREAM_SUMMARY_SQL`,
  `SELECT_STREAM_META_SQL`, `UPSERT_STREAM_META_SQL`,
  `TRIM_STREAM_FRAMES_SQL`, `COUNT_STREAM_FRAMES_SQL`.
- `crates/ff-backend-sqlite/src/queries/exec_core.rs`:
  `UPDATE_EXEC_CORE_PROGRESS_SQL` added for the `progress` path.
- `crates/ff-backend-sqlite/Cargo.toml`: direct `serde_json` dep added
  (summary DurableSummary path merges in Rust before write-back).
- **`crates/ff-backend-sqlite` — Phase 2a.2 hot-path trait impls (RFC-023).**
  Replaces the `Unavailable` stubs for `claim`, `complete`, and `fail`
  with real bodies paralleling `ff-backend-postgres::attempt`. Handle
  minting + decode flows through the Phase 2a.1.5 `handle_codec`
  (`BackendTag::Sqlite`, wire byte `0x03`); fence CAS is enforced under
  `BEGIN IMMEDIATE` txns (§4.1 A3); capability subset-match reads the
  `ff_execution_capabilities` junction (§4.1 A4) and compares in Rust
  via `ff-core::caps`. Transient `SQLITE_BUSY` is wrapped by the
  Phase-2a.1 `retry_serializable` helper (`MAX_ATTEMPTS = 3`,
  5ms/10ms backoff). Phase 2a.3 (lease-lifecycle: renew, delay,
  cancel, wait_children, append_frame) follows; no `Supports`-flag
  flips land in this PR (§4.4 Rev-6 clarification: `claim` /
  `complete` / `fail` are trait-mandatory hot-path ops without a
  `Supports` bool).
- `crates/ff-backend-sqlite/src/queries/attempt.rs` +
  `.../queries/exec_core.rs` populated with hot-path SQL consts
  (`SELECT_ELIGIBLE_EXEC_SQL`, `UPSERT_ATTEMPT_ON_CLAIM_SQL`,
  `SELECT_ATTEMPT_EPOCH_SQL`, `UPDATE_ATTEMPT_COMPLETE_SQL`,
  `UPDATE_ATTEMPT_FAIL_RETRY_SQL`, `UPDATE_ATTEMPT_FAIL_TERMINAL_SQL`,
  `INSERT_COMPLETION_EVENT_SQL`, `UPDATE_EXEC_CORE_CLAIM_SQL`,
  `UPDATE_EXEC_CORE_COMPLETE_SQL`, `UPDATE_EXEC_CORE_FAIL_RETRY_SQL`,
  `UPDATE_EXEC_CORE_FAIL_TERMINAL_SQL`).
- `ff-backend-sqlite::errors::map_sqlx_error` + `IsRetryableBusy for
  EngineError` — lets `retry_serializable` classify translated
  transport errors without a custom error type.
- `ff-backend-sqlite`: claim path now walks a bounded batch (16) of
  eligible rows and matches capabilities in-Rust until it finds the
  first serveable one — fixes a starvation window where a
  high-priority row with unserveable caps would block the entire
  lane for a given worker (PR-375 review).
- `ff-backend-sqlite`: lease-lifecycle outbox parity with PG — claim
  / complete / fail emit `ff_lease_event` rows (`acquired` /
  `revoked`) under the same txn so a later
  `subscribe_lease_history` reader sees the cascade (PR-375 review).
- `ff-backend-sqlite`: `FailureClass` wildcard defaults to retry
  (least-destructive) per the project's `#[non_exhaustive]` rule —
  an unknown future variant MUST NOT silently burn the attempt on
  backend upgrades.
- `crates/ff-backend-sqlite/tests/hot_path.rs` — 10 tests covering
  claim happy path, claim no-work, capability-subset mismatch
  skip-return-None, capability-subset-walk-past-priority, claim +
  complete lease-event emission, complete happy path, complete
  fence-mismatch → `Contention(LeaseConflict)`, cross-backend
  (Valkey-tagged) handle rejection via
  `ValidationKind::HandleFromOtherBackend`, transient-fail retry
  scheduling, and permanent-fail terminal.
- **`crates/ff-backend-sqlite` — SQLite dev-only backend (RFC-023 Phase 1a).**
  Activation requires `FF_DEV_MODE=1`. Construction is guarded at the
  `SqliteBackend::new` TYPE level (§4.5 / §3.3 A3) so every entry
  point — embedded, `ff_server::start_sqlite_branch`, and
  `ff_sdk::FlowFabricWorker::connect_with` — pays the guard.
  Phase 1a ships the scaffolding only: process-local per-path
  registry (§4.2 B6), `sqlx::SqlitePool`, WARN banner on
  construction, and an `EngineBackend` impl whose data-plane
  methods return `EngineError::Unavailable`. Phase 1b lands the
  hand-ported SQLite-dialect migrations; Phase 2+ replaces the stubs.
- `BackendKind::Sqlite` variant on `ff_server::config::BackendKind`.
- `SqliteServerConfig` (public, `#[non_exhaustive]`) with `new(path)`
  + `with_pool_size(n)` + `with_wal(bool)` builders.
- `ServerConfig::sqlite_dev()` — zero-config test/dev constructor
  (in-memory SQLite, auth disabled, `127.0.0.1:0` bind).
- `SqliteBackend::new(path)` embedded constructor returning
  `Result<Arc<Self>, BackendError>`.
- `BackendError::RequiresDevMode` variant (RFC-023 §4.5). Render
  text matches the server-path §3.3 text (docs URL included).
- `ServerError::SqliteRequiresDevMode(BackendError)` variant
  wrapping the embedded-path refusal.
- `FF_BACKEND=sqlite`, `FF_SQLITE_PATH`, `FF_SQLITE_POOL_SIZE`, and
  `FF_DEV_MODE` env vars on `ServerConfig::from_env`. README +
  rustdoc env tables updated.
- CI cell (`cargo check -p ff-sdk --no-default-features --features
  sqlite`) — mechanical regression guard for the RFC-023 Phase 1a
  worker.rs cfg-gate discipline.
- **`ff-backend-sqlite` — Phase 1b: 14 hand-ported SQLite-dialect
  migrations (0001-0014) covering exec / attempt / flow / budget /
  quota / outbox schemas.** `SqliteBackend::new` now calls
  `sqlx::migrate!` against the pool on construction. Flat tables (no
  HASH-partitioning DDL), `jsonb → TEXT`, `bytea → BLOB`,
  `uuid → BLOB`, `BIGSERIAL → INTEGER PRIMARY KEY AUTOINCREMENT`,
  `pg_notify` triggers dropped (broadcast moves to Rust per RFC-023
  §4.2), `CREATE INDEX CONCURRENTLY` / `USING GIN` / `INCLUDE(...)`
  collapse to plain `CREATE INDEX`. Trait impls remain `Unavailable`
  until Phase 2+.
- `ff_execution_capabilities` junction table on SQLite (replaces the
  PG `text[]` + GIN shape per RFC-023 §4.1 A4). `WITHOUT ROWID` +
  reverse index for capability-first routing lookups.
- `scripts/lint-migrations.sh` — parity-drift lint comparing the PG
  and SQLite migration directories. Wired into `.github/workflows/
  matrix.yml` as a quality-gate job (`migration-parity`).
- **`ff-backend-sqlite` — Phase 2a.1: dialect-fork foundations +
  retry helper + classifier (RFC-023 §4.3).** `src/queries/`
  scaffolding (`attempt`, `exec_core`, `lease`, `dispatch`
  submodules) establishes the dialect-forked query module layout;
  method bodies land in Phase 2a.2 / 2a.3. `src/retry.rs` adds
  `retry_serializable` helper + `IsRetryableBusy` trait, mirroring
  the PG reference (`5ms * 2^attempt` backoff, `MAX_ATTEMPTS = 3`).
  `is_retryable_sqlite_busy` + `MAX_ATTEMPTS` promoted to the public
  surface so Wave-9 ops can pull the full retry vocabulary from a
  single path. Classifier + retry-loop unit tests (11 total) cover
  busy / locked / corrupt / full / misuse / non-DB errors plus first-
  try success, mid-retry recovery, exhaustion, and backoff shape.
- **`ff-core` — `BackendTag::Sqlite` variant (wire byte `0x03`)
  (RFC-023 Phase 2a.1.5 / §4.4 item 11).** Additive, non-breaking —
  the enum is already `#[non_exhaustive]`. `wire_byte` /
  `from_wire_byte` gain the `0x03 ↔ Sqlite` mapping; `handle_codec`
  v2 encode/decode round-trips SQLite-tagged handles through the
  existing core codec with no new infrastructure.
- **`ff-backend-sqlite` — new `handle_codec` module (RFC-023 §4.5 /
  Phase 2a.1.5 scaffold).** Mirrors the `ff-backend-postgres` and
  `ff-backend-valkey` wrappers: `encode_handle` mints
  SQLite-tagged `Handle`s via `ff_core::handle_codec::encode`,
  `decode_handle` validates the outer `BackendTag` + embedded tag
  and rejects Valkey/Postgres-minted handles with
  `ValidationKind::HandleFromOtherBackend`. Bodies are
  `#[allow(dead_code)]` through Phase 2a.1.5 — Phase 2a.2 wires
  them into the claim/complete/fail path.

### Changed

- **`ServerError` is now `#[non_exhaustive]`** (RFC-023 §8 minor
  break). Consumers with exhaustive matches must add a wildcard
  arm.
- **`ServerConfig` is now `#[non_exhaustive]`** (RFC-023 §8 minor
  break) + gained a `sqlite: SqliteServerConfig` field. Struct-
  literal consumers migrate to `ServerConfig::from_env()`,
  `ServerConfig::sqlite_dev()`, or mutation from
  `ServerConfig::default()`.
- **`ff_sdk::FlowFabricWorker` surface under `--no-default-features,
  features = ["sqlite"]`** (RFC-023 §4.4 item 10). The
  `ff_sdk::worker` module is no longer gated on `valkey-default`;
  `FlowFabricWorker::connect_with(config, backend, completion)` is
  always reachable and no longer dials Valkey internally (no PING,
  no alive-key SET-NX, no `ff:config:partitions` HGETALL).
  Valkey-specific methods — `connect`, `claim_next`,
  `claim_execution`, `claim_from_grant`, `claim_via_server`,
  `claim_from_reclaim_grant`, `claim_resumed_execution`,
  `read_execution_context`, `deliver_signal` — are now
  `#[cfg(feature = "valkey-default")]` and ABSENT under
  sqlite-only features. A backend-agnostic SDK claim/signal loop
  is deferred to a future RFC. The embedded `ferriskey::Client`
  field is `Option<Client>`; populated by `connect`, left `None`
  by `connect_with`.

## [0.11.0] - 2026-04-26

### Added

- **Postgres Wave 9 — 15 trait methods flipped from `Unavailable` to
  real impl, 12 capability flags flipped `false → true` (RFC-020
  Rev 7).** Methods: `cancel_execution`, `change_priority`,
  `replay_execution`, `revoke_lease`, `read_execution_state`,
  `read_execution_info`, `get_execution_result`, `create_budget`,
  `reset_budget`, `create_quota_policy`, `get_budget_status`,
  `report_usage_admin`, `list_pending_waitpoints`,
  `cancel_flow_header`, `ack_cancel_member` all land concretely on
  `PostgresBackend`. Capability flags (per
  `PostgresBackend::capabilities().supports`): the 7 operator-control
  + read-model method-level flags (`cancel_execution`,
  `change_priority`, `replay_execution`, `revoke_lease`,
  `read_execution_state`, `read_execution_info`,
  `get_execution_result`), the 2 grouping flags (`budget_admin`
  covering the 4 budget methods, `quota_admin` covering
  `create_quota_policy`), `list_pending_waitpoints`,
  `cancel_flow_header`, `ack_cancel_member` — **12 flags total** —
  all flip to `true`. `subscribe_instance_tags` remains `false` per
  #311 (speculative demand; served by `list_executions` +
  `ScannerFilter::with_instance_tag`). Design record:
  `rfcs/RFC-020-postgres-wave-9.md`.
- **Migration 0010 — `ff_operator_event` outbox.** New LISTEN/NOTIFY
  channel for operator-control events (`priority_changed` /
  `replayed` / `flow_cancel_requested`) emitted by the Postgres impls
  of `change_priority` / `replay_execution` / `cancel_flow_header`.
  Preserves the RFC-019 `ff_signal_event` subscriber contract
  (separate channel, no repurpose).
- **Migration 0011 — `ff_waitpoint_pending` additive columns.**
  Adds `state`, `required_signal_names`, `activated_at_ms` to serve
  the real `PendingWaitpointInfo` contract from Postgres.
  Producer-side `suspend_ops` writes the new columns on initial
  insert + the pending→active activation transition.
- **Migration 0012 — `ff_quota_policy` family.** 3 tables
  (`ff_quota_policy`, `ff_quota_window`, `ff_quota_admitted`), 256-way
  HASH-partitioned on `partition_key`. Concurrency represented as a
  column on `ff_quota_policy` (Valkey-counter-key → Postgres column
  collapse).
- **Migration 0013 — `ff_budget_policy` additive columns.**
  `next_reset_at_ms BIGINT` + 4 breach-tracking columns + 3
  definitional columns, backing both `get_budget_status` and the new
  `BudgetResetReconciler`. `report_usage_impl` extended to maintain
  `breach_count` / `soft_breach_count` / `last_breach_at_ms` /
  `last_breach_dim` incrementally, matching Valkey's existing
  pattern.
- **Migration 0014 — `ff_cancel_backlog` table.** Per-member cancel
  backlog driving `cancel_flow_header` + `ack_cancel_member` on
  Postgres and the cancel-backlog reconciler sweep.
- **`BudgetResetReconciler` (Postgres-native).** New scanner under
  `crates/ff-backend-postgres/src/reconcilers/budget_reset.rs`;
  selects due resets by `next_reset_at_ms <= now`. Column-on-policy
  scheduling rather than a separate schedule table.

### Changed

- **POSTGRES_PARITY_MATRIX.md.** 13 Wave-9 rows flipped `stub → impl`
  on the Postgres column. v0.11.0 parity summary section added. Every
  Stage-A row is now `impl | impl` except `fetch_waitpoint_token_v07`
  (retired at v0.8.0) and `subscribe_instance_tags` (`n/a` on both).

### Fixed

- `rotate_waitpoint_hmac_secret_all` matrix-row label updated to
  reflect its long-standing `impl | impl` status (housekeeping per
  RFC-020 §3.3; no behaviour change).

### Notes

- No Rust API surface change. No wire-format change. Valkey backend
  behaviour unchanged (RFC-020 §5.2 "no Valkey behavior change"
  mandate).
- **Required before upgrade on Postgres deployments:** apply
  migrations 0010–0014 before serving v0.11.0 traffic. Auto-run by
  `ff-server` at boot; operators applying out-of-band must run them
  in order. See `docs/CONSUMER_MIGRATION_0.11.md`.

## [0.10.0] - 2026-04-26

### Added

- `EngineBackend::suspend_by_triple(exec_id, LeaseFence, SuspendArgs)`
  — service-layer entry point for suspending an execution when the
  caller holds a lease fence triple but no worker `Handle` (cairn #322;
  unblocks cairn's pause-by-operator, enter-waiting-approval, and
  cancel-with-timeout-record call sites that today fall back to raw
  FCALL glue). Additive with a default impl returning
  `EngineError::Unavailable { op: "suspend_by_triple" }`; Valkey and
  Postgres backends override. Semantics mirror `suspend` (same
  `SuspendArgs` validation, same `SuspendOutcome` lifecycle, same
  RFC-013 §3 replay / dedup contract) — the only difference is the
  fencing source. On Valkey the triple's `(lease_id, lease_epoch,
  attempt_id)` drives the Lua fence against `lease_current`;
  `attempt_index` + `lane_id` + `worker_instance_id` are recovered
  from `exec_core`. On Postgres the fence is `lease_epoch` against
  `ff_attempt`; `attempt_index` is read from `ff_exec_core` (Postgres
  attempts are keyed by `(execution_id, attempt_index)` — the triple's
  `attempt_id` is advisory on this backend).
- `ff_core::handle_codec::v1_handle_for_tests` — test-only fixture
  that synthesises a pre-Wave-1c (v1) handle byte buffer for the
  given `HandlePayload`. Gated behind the new `test-fixtures` feature
  on `ff-core` (default OFF) so it never leaks into production
  builds. Round-trips through `handle_codec::decode` via the v1
  compat path, yielding `BackendTag::Valkey` + the original payload.
  Motivated by downstream cross-version compat testing (cairn #323)
  where event logs persisted under FF 0.3 must still decode under
  FF 0.9+.
- `ff_core::stream_events` module: typed event enums + per-family
  subscription aliases for the four `EngineBackend::subscribe_*`
  methods (RFC-019 Stage C; addresses #282 typed surface gap).
  - `LeaseHistoryEvent` with `Acquired` / `Renewed` / `Expired` /
    `Reclaimed` / `Revoked` variants.
  - `CompletionEvent` + `CompletionOutcome::{Success, Failure,
    Cancelled, Other(String)}`.
  - `SignalDeliveryEvent` + `SignalDeliveryEffect::{Satisfied,
    Buffered, Deduped, Other(String)}`.
  - `InstanceTagEvent` with `Attached` / `Cleared` variants (trait
    surface only; producer wiring still deferred per #311 audit).
  Each family ships a subscription alias
  (`LeaseHistorySubscription`, `CompletionSubscription`,
  `SignalDeliverySubscription`, `InstanceTagSubscription`). All
  enums + structs are `#[non_exhaustive]` so future additions are
  non-breaking; structs pair with `new()` constructors so external
  crates (ff-backend-valkey, ff-backend-postgres) can still build
  instances.
- `subscribe_signal_delivery` implemented on both backends
  (RFC-019 Stage B; closes #310). Valkey: XREAD BLOCK on partition
  aggregate stream `ff:part:{fp:N}:signal_delivery` (producer XADD
  added to `ff_deliver_signal` at new KEYS[15] slot). Postgres:
  `ff_signal_event` outbox + `NOTIFY ff_signal_event` trigger
  (migration 0007); producer INSERT lives in the `deliver_signal`
  SERIALIZABLE transaction.
- `ValkeyBackend::subscribe_completion` (RFC-019 Stage B; closes #309).
  Pubsub-backed (at-most-once over live subscription window), cursor
  always empty. Durable Postgres impl was Stage A; durable Valkey
  impl is follow-up (not in Stage B scope).
- `PostgresBackend::subscribe_lease_history` (RFC-019 Stage B;
  closes #308). `ff_lease_event` outbox table + `NOTIFY
  ff_lease_event` trigger; subscription wraps `PgListener` with
  cursor `0x02 ++ event_id(BE8)`. Valkey already impl'd at Stage A.

### Changed

- **Breaking for direct `EngineBackend` impls**: the four
  `subscribe_*` trait methods now return family-specific typed
  subscriptions instead of `StreamSubscription<StreamEvent>` with a
  byte payload. Trait consumers match on typed variants directly;
  no more NUL-delimited / JSON payload parsing. The untyped
  `StreamEvent` + `StreamSubscription` + `StreamFamily` types are
  removed from `ff_core::stream_subscribe`; cursor codec
  (`StreamCursor` + `encode_valkey_cursor` / `decode_valkey_cursor`
  / `encode_postgres_event_cursor` / `decode_postgres_event_cursor`)
  stays unchanged. See
  `docs/CONSUMER_MIGRATION_typed-subscribe-events.md` for the
  consumer migration snippet.
- **Breaking for direct `EngineBackend` impls**:
  `subscribe_lease_history`, `subscribe_completion`, and
  `subscribe_signal_delivery` gain a required `filter:
  &ScannerFilter` parameter (#282 / RFC-019 surface). Pass
  `&ScannerFilter::default()` to preserve unfiltered behaviour; pass
  `ScannerFilter::new().with_instance_tag(k, v)` to isolate a
  subscriber to a single cairn-style instance when multiple
  FlowFabric consumers share a backend. Valkey reuses the #122
  `FilterGate` (per-event HGET on
  `ff:exec:{p}:<eid>:tags`); Postgres filters inline against new
  denormalised `namespace` + `instance_tag` columns on
  `ff_lease_event` (migration 0008) and `ff_signal_event` (migration
  0009). Return types are unchanged — filtering happens inside the
  backend stream before yielding, not via an envelope wrapper.
- **BREAKING (direct consumers of v0.9 capability API):**
  `EngineBackend::capabilities_matrix() -> CapabilityMatrix` renamed
  to `capabilities() -> Capabilities`; `BTreeMap<Capability,
  CapabilityStatus>` replaced by flat `Supports` struct with named
  bool fields (e.g. `caps.supports.cancel_execution`). Matches cairn's
  original #277 ask. Partial-status nuance (e.g. non-durable cursor on
  Valkey `subscribe_completion`) moved to rustdoc on the trait method
  and `docs/POSTGRES_PARITY_MATRIX.md`. Consumers that went through
  `flowfabric::core::capability::CapabilityMatrix` update their import
  to `Capabilities` + dot-access fields.
- `subscribe_instance_tags` deferred per audit (#311): `list_executions`
  + `ScannerFilter` + pagination meets cairn's `instance_tag_backfill`
  use case (one-shot backfill, not realtime tail). Trait method remains
  (returns `Unavailable` on both backends); reserving the surface for
  future concrete demand. RFC-019 §instance_tags amended.

## [0.9.0] - 2026-04-25

v0.9 is **fully additive** on top of v0.8: no consumer source changes
are required. The release lets consumers *delete* adapter code — one
umbrella-crate pin instead of seven ff-* pins, trait-level HMAC-secret
seeding instead of raw HSET, trait-level boot prep instead of
`BackendKind::Valkey` branches. Closes every consumer-filed issue
since v0.8 except the RFC-019 Stage B follow-ups (#308–#311,
intentional scope).

### Added

- **`EngineBackend::capabilities_matrix()`** + `BackendIdentity` +
  `Capability` enum + `CapabilityStatus` enum + `CapabilityMatrix`
  type. Consumers can query a backend's supported operations at
  runtime before dispatch; gated UI features + alternative code
  paths no longer need to trap `EngineError::Unavailable` after
  the fact. RFC-018 Stage A; Stage B (derived parity matrix) +
  Stage C (HTTP `GET /v1/capabilities`) land in follow-ups.
  Closes #277.
- **`EngineBackend::subscribe_{lease_history,completion,signal_delivery,instance_tags}`**
  trait methods returning `Stream<Item = Result<StreamEvent, EngineError>>`.
  `subscribe_lease_history` implemented on Valkey (dedicated-connection
  `XREAD BLOCK` against `ff:part:{fp:N}:lease_history`);
  `subscribe_completion` implemented on Postgres (wraps the existing
  `ff_completion_event` outbox + `LISTEN ff_completion` machinery).
  Cursor is opaque `bytes::Bytes` with a backend-family + version
  prefix byte; consumer-side reconnect on
  `EngineError::StreamDisconnected { cursor }`. Four-family allow-list
  (owner-adjudicated, RFC-019 §Open Questions #5). Other family ×
  backend combinations return `Unavailable` and are tracked in the
  RFC-019 Stage B follow-up issues. RFC-019 Stage A; Stage B
  (complete the family × backend matrix + ff-engine
  `completion_listener` migration) + Stage C (HTTP SSE surface) land
  in follow-ups. Closes #282.
- **`ValkeyBackend::with_embedded_scheduler`** public constructor —
  lets direct `Arc<dyn EngineBackend>` consumers (not going through
  `ff-server`) reach `EngineBackend::claim_for_worker`. Closes #293.
  Consumers that talk to a running ff-server via HTTP continue to
  use `FlowFabricWorker::claim_via_server` and don't need the
  embedded scheduler.
- `EngineBackend::cancel_flow` now supports `WaitTimeout(Duration)`
  and `WaitIndefinite` on both Valkey and Postgres backends via a
  shared `describe_flow` poll loop. Callers that previously needed
  client-side `NoWait` + poll can now pass the wait mode through
  the trait. Closes #298.
- **`flowfabric` umbrella crate** — re-exports the ff-* family
  (`ff_core`, `ff_sdk`, and optionally `ff_backend_valkey`,
  `ff_backend_postgres`, `ff_engine`, `ff_scheduler`, `ff_script`)
  behind feature flags. Consumers can pin one crate + feature-flag
  the backend instead of tracking 7–8 separate pins in lockstep.
  Default `features = ["valkey"]` preserves the v0.7–v0.8 stability
  posture; `default-features = false, features = ["postgres"]` opts
  into the Postgres backend. See
  [`docs/MIGRATIONS.md`](docs/MIGRATIONS.md)
  § "Umbrella crate (flowfabric)" for before/after. Closes #279.
- `ff-sdk` re-exports `ClaimGrant`, `ReclaimGrant`, `ClaimPolicy`, and
  `ReclaimToken`; consumers typing `claim_from_grant` /
  `claim_from_reclaim_grant` signatures can drop their direct
  `ff-scheduler` dep pin (closes #283). `Scheduler` itself is
  intentionally not re-exported — implementing a scheduler stays
  behind the explicit `ff-scheduler` dep.
- `LeaseSummary` gains `lease_id`, `attempt_index`, `last_heartbeat_at`
  fields + fluent `with_lease_id` / `with_attempt_index` /
  `with_last_heartbeat_at` setters (closes #278). `#[non_exhaustive]`
  preserved; construct via `LeaseSummary::new(..).with_*(..)`. Valkey
  backend surfaces all three via `describe_execution`, letting
  downstream consumers delete their HGET-based lease-summary
  wrappers.
- **`EngineBackend::seed_waitpoint_hmac_secret`** — trait-level initial
  HMAC secret seed (closes #280). Idempotent per-partition; returns
  `SeedOutcome::{Seeded, AlreadySeeded { same_secret }}`. Valkey impl
  fans out across partitions with HGET probe + HSET install; Postgres
  impl single-INSERT against `ff_waitpoint_hmac`. Lets consumers drop
  raw HSET boot paths that blocked PG adoption.
- **`EngineBackend::prepare`** — trait-level one-time boot preparation
  step (closes #281). Valkey delegates to `ff_script::loader::ensure_library`
  (FUNCTION LOAD + retry), Postgres returns `NoOp` (migrations run
  out-of-band). Idempotent; safe on every boot. Lets consumers drop
  backend-aware `if let BackendKind::Valkey = ...` boot branches.
- **`examples/token-budget`** — v0.9 UC-37 + UC-39 demo exercising
  flow-level token budget + per-attempt `report_usage`, including
  hard-breach → `cancel_flow`. ~200 lines; consumer-facing docs for
  the v0.9 surface (LeaseSummary fields, prepare, seed_waitpoint,
  umbrella-crate imports).
- **`scripts/published-smoke.sh`** — published-artifact smoke harness
  that scratch-compiles a consumer project against the just-published
  crate versions. Release-gate blocker per CLAUDE.md §5 (caught
  v0.3.2 → v0.6 external-consumer breakage). Runs against the
  flowfabric umbrella crate's v0.9 symbol set.
- **`docs/MIGRATIONS.md`** — rolling 3-minor-window migration page
  (v0.7 / v0.8 / v0.9). Replaces per-release `CONSUMER_MIGRATION_*.md`
  files; release gate validates it on every tag.
- **`CLAUDE.md` §5 Release Gate** — codified the 8-item pre-tag gate
  (smoke, example builds, live-run, headline example, README sweep,
  CHANGELOG finalize, POSTGRES_PARITY_MATRIX current, pre-publish
  smoke in release.yml). Reinforces the lessons from v0.3/v0.6/v0.8
  partial-publish recoveries.

### Changed

- `docs/CONSUMER_MIGRATION_v0.8.md` → replaced by rolling
  `docs/MIGRATIONS.md` covering the last 3 minor versions
  (v0.7/v0.8/v0.9). Release gate (CLAUDE.md §5 item 5) validates
  it on every tag.

### CI

- **Matrix-CI Postgres cell** — ubuntu-latest × postgres 16 × standalone
  matrix cell added (#299). Gates Postgres backend parity on every PR
  alongside the existing Valkey standalone + cluster cells. Runs
  `ff-backend-postgres` suite with `--test-threads=1` for SERIALIZABLE
  budget reasons.

### Fixed

- **`ff-backend-postgres` `suspend_signal` keystore race.** Tests
  used to TRUNCATE the global `ff_waitpoint_hmac` table in per-test
  setup; under parallel execution test A's TRUNCATE wiped the kids
  test B had just seeded. Tests now route the keystore through a
  per-test Postgres schema (`ffpg_test_<uuid>`) via pool
  `search_path`, so rotate / seed tests don't race. Keystore-only
  tests parallelize cleanly; matrix-CI Postgres cell keeps
  `--test-threads=1` for suspend/deliver SERIALIZABLE budget
  reasons unrelated to the keystore. Closes #301.
- **ferriskey slot-refresh throttle** uses `Instant::now()` instead of
  `SystemTime::now()` for elapsed-time measurement. `Instant` is
  monotonic; immune to NTP steps, VM suspend/restore, and variable
  clock resolution. Closes #290.
- **Cluster-topology bootstrap race (issue #275).** On a just-formed
  6-node Valkey cluster, `FUNCTION LOAD` could route to a node whose
  topology view still classified it as primary even though it had
  already transitioned to replica in gossip — surfaced as `READONLY`
  during `Server::start_with_metrics`. Two-part fix:
  - `.github/cluster/bootstrap.sh` now polls `cluster_state:ok` on all
    6 nodes and verifies 3 masters + 3 slaves from each node's
    perspective before declaring the cluster ready (closes #276).
  - `ff_script::loader::ensure_library` retry window extended from
    3 × 1s = 4s to 6 attempts × 1s/2s/4s/4s/4s = 15s. READONLY is
    treated as transient; the extended window covers gossip-convergence
    + slot-map-refresh even on slow CI hardware (closes #289).
  Follow-up ferriskey robustness work tracked at #290 (swap
  `SystemTime::now` → `Instant::now` in slot-refresh throttling).

## [0.8.1] - 2026-04-25

Release-infrastructure fix for v0.8.0 partial-publish.

### Fixed

- **Publish order corrected.** `ff-scheduler` now publishes before
  `ff-backend-valkey` (RFC-017 Stage C added `ff-backend-valkey →
  ff-scheduler` dep; publish-list wasn't updated to match). The
  v0.8.0 tag partial-published ferriskey + ff-core + ff-script +
  ff-observability + ff-observability-http; those were yanked and
  re-released at 0.8.1.
- **Smoke gate env-override.** `FF_SMOKE_FANOUT_P99_MS` allows CI
  hardware variance without hiding perf regressions locally (master
  spec 500ms stays strict; CI runners use 1200ms). Release-workflow
  smoke now passes on GitHub-hosted runners.

No functional changes vs 0.8.0; all RFC-017 delivery notes from 0.8.0
below still apply.

## [0.8.0] - 2026-04-24

RFC-017 Wave 8 — Postgres backend ships first-class. The `EngineBackend`
trait expanded by ~17 methods across Waves 0–8, `ServerConfig` flattened
into a sum-typed `BackendConfig`, and the v0.7 `/v1/pending-waitpoints`
compatibility shim was retired. See
[`docs/CONSUMER_MIGRATION_v0.8.md`](docs/CONSUMER_MIGRATION_v0.8.md) for
the full migration guide and
[`docs/POSTGRES_PARITY_MATRIX.md`](docs/POSTGRES_PARITY_MATRIX.md) for
the v0.8.0 Postgres parity audit.

### Breaking

- **RFC-017 `EngineBackend` trait expansion.** ~17 methods added over
  Waves 4–8 to cover the flow/attempt/budget/admin/completion families
  on Postgres. Custom `EngineBackend` implementations must add the new
  methods; `HookedBackend` / `PassthroughBackend` / `ValkeyBackend` /
  `PostgresBackend` all carry in-tree impls.
- **`ServerConfig` flat Valkey fields removed.** The deprecated
  `host` / `port` / `tls` / `cluster` / `skip_library_load` fields on
  `ServerConfig` are gone. Replaced by nested
  `ServerConfig::valkey: ValkeyServerConfig` mirroring the existing
  `ServerConfig::postgres: PostgresServerConfig` shape.
- **`PendingWaitpointInfo` wire-shape change.** The
  `/v1/executions/{id}/pending-waitpoints` response no longer carries a
  raw `waitpoint_token` HMAC. Clients correlate via `(token_kid,
  token_fingerprint)` alongside the other sanitised fields. The
  `Deprecation: ff-017` response header was also removed.
- **`ServerError` variant additions.** Additive variants landed across
  Stages D1–E3 for Postgres-path error classification; `ServerError` is
  `#[non_exhaustive]`, so consumers matching it must add a catch-all
  arm if they haven't already.
- **`ClaimPolicy::immediate()` removed / `ClaimPolicy::new` signature
  change** (from the v0.7 cycle, published here). `ClaimPolicy` now
  carries `worker_id`, `worker_instance_id`, `lease_ttl_ms`.
  `ReclaimToken::new` gained the matching identity args.

### Added

- **RFC-017 Stage E4: Postgres backend first-class over HTTP.**
  `FF_BACKEND=postgres` boots natively — the v0.7-era
  `FF_BACKEND_ACCEPT_UNREADY=1` / `FF_ENV=development` dev-override is
  gone and `BACKEND_STAGE_READY` now lists both `valkey` and
  `postgres`.
- **`PostgresScheduler` + 6 reconcilers.** Stage E3 landed the
  Postgres execution-dispatch scheduler plus the sibling-cancel,
  lease-timeout, completion-listener, cancel-backlog, flow-staging,
  and edge-group-policy reconcilers on top of the Wave 3 schema.
- **`Server::start_with_backend`** — test-injection entry point that
  accepts a constructed `EngineBackend` trait object, used by
  `crates/ff-test/tests/http_postgres_smoke.rs` for the integration
  smoke without a full env-var dance.
- **`http_postgres_smoke` integration test** — end-to-end POST/GET
  coverage of the HTTP API against a `PostgresBackend`, gated by the
  `postgres-e2e` feature.
- **New `docs/CONSUMER_MIGRATION_v0.8.md`** — upgrade guide for
  `ServerConfig`, `PendingWaitpointInfo`, `FF_BACKEND=postgres`
  setup, and the `EngineBackend` trait expansion.

### Removed

- `Server::client()` accessor / `Server::client` field.
- `Server::fcall_with_reload` inherent.
- `Server::scheduler` field (replaced by trait-routed dispatch).
- Legacy `waitpoint_token` wire field on
  `/v1/executions/{id}/pending-waitpoints`; the `Deprecation: ff-017`
  response header; and the `ValkeyBackend::fetch_waitpoint_token_v07`
  inherent + trait method. The
  `ff_pending_waitpoint_legacy_token_served_total` and
  `ff_backend_unready_boot_total` counters remain declared in
  `ff-observability` for historical continuity but are no longer
  emitted by the server.
- Deprecated `ServerConfig` flat Valkey fields (see Breaking above).
- `FF_BACKEND_ACCEPT_UNREADY` + `FF_ENV` dev-mode escape hatches on
  the server boot path.

### v0.7-cycle changes (shipped in 0.8.0)

The following items landed during the v0.7 RFC cycle but did not get a
point release of their own; they ship here.

#### Added

- **RFC-v0.7 Wave 4c: Postgres flow family (`EngineBackend`).**
  `PostgresBackend` now implements six flow-scoped trait methods over
  the Wave 3 schema: `describe_flow`, `list_flows`, `list_edges`,
  `describe_edge`, `cancel_flow`, and `set_edge_group_policy`.
  `cancel_flow` runs a SERIALIZABLE cascade with a 3-attempt retry
  loop (Q11); exhaustion surfaces as
  `EngineError::Contention(ContentionKind::RetryExhausted)` so
  consumers fall back to the cancel-backlog reconciler.
- **`ContentionKind::RetryExhausted`** — additive pre-1.0 variant on
  the `#[non_exhaustive]` `ContentionKind` enum. Classified as
  `Retryable` via the existing blanket `Contention(_)` arm.

- **RFC-v0.7 Wave 2: Valkey `EngineBackend::claim` +
  `claim_from_reclaim` implementations.** `ValkeyBackend::claim` now
  performs a fresh-find scan across execution partitions — ZRANGEBYSCORE
  the lane's eligible set, capability-subset filter via
  `ff_core::caps::matches`, issue `ff_issue_claim_grant`, and claim via
  `ff_claim_execution` — and returns a `HandleKind::Fresh` handle.
  `claim_from_reclaim` unwraps the `ReclaimGrant` carried by
  `ReclaimToken` and routes through `ff_claim_resumed_execution`,
  returning a `HandleKind::Resumed` handle with a bumped lease epoch.
  Both replace the prior `EngineError::Unavailable` stubs.

### Changed

- **Pre-1.0 BREAKING — `ClaimPolicy` extended with worker identity.**
  `ClaimPolicy` now carries `worker_id: WorkerId`,
  `worker_instance_id: WorkerInstanceId`, and `lease_ttl_ms: u32` so the
  backend's `claim(&self, lane, caps, policy)` can invoke
  `ff_claim_execution` without a side-channel identity lookup (Wave 2
  design-gap adjudication). The prior `ClaimPolicy::immediate()` and
  `ClaimPolicy::with_max_wait(..)` constructors are replaced by a single
  `ClaimPolicy::new(worker_id, worker_instance_id, lease_ttl_ms,
  max_wait)`. Call-sites migrated: `ff-core::backend` tests,
  `ff-sdk::layer::tracing_layer` test. Generic forwarders
  (`ff-sdk::layer::hooks`, `ff-sdk::layer::test_support`,
  `ff-backend-postgres::claim` stub) pass `ClaimPolicy` through
  unchanged.
- **Pre-1.0 BREAKING — `ReclaimToken` extended with worker identity.**
  Mirrors the `ClaimPolicy` shape so `claim_from_reclaim` (which does
  not take a `ClaimPolicy`) has the identity it needs to invoke
  `ff_claim_resumed_execution`. New constructor:
  `ReclaimToken::new(grant, worker_id, worker_instance_id, lease_ttl_ms)`.

- **RFC-v0.7 Wave 1b: `EngineBackend::rotate_waitpoint_hmac_secret_all`
  (migration-master Q4).** New additive trait method for cluster-wide
  waitpoint HMAC kid rotation. Valkey impl fans out one
  `ff_rotate_waitpoint_hmac_secret` FCALL per execution partition and
  returns per-partition outcomes. Postgres impl (Wave 4) will resolve
  to a single INSERT into the global `ff_waitpoint_hmac(kid, secret,
  rotated_at)` table; Wave 1b lands the stub returning
  `EngineError::Unavailable { op: "pg.rotate_waitpoint_hmac_secret_all" }`.
  SDK exposes `FlowFabricWorker::rotate_waitpoint_hmac_secret_all`;
  HookedBackend + PassthroughBackend dispatch the new method. The
  pre-existing per-partition free-fn
  `ff_sdk::admin::rotate_waitpoint_hmac_secret_all_partitions` is
  unchanged for backwards compat.
- **`ff_core::waitpoint_hmac` re-export module.** Consumer-facing
  import path for the waitpoint HMAC wire types (`WaitpointHmac`,
  `VerifyingKid`, `WaitpointHmacKids`) + rotation args/outcomes.
  Signing and verification remain server-side (Lua on Valkey; Wave 4
  stored procs on Postgres); the module doc captures the
  sign/verify-location contract so external crates have one stable
  path to consume from.

### Changed

- **RFC-v0.7 Wave 1a: `ff-core::caps` extracted for backend-shared
  capability matching.** The capability subset predicate previously lived
  as `ff_scheduler::claim::caps_subset` (private) and was implicitly
  duplicated by the Lua authority in `lua/scheduling.lua`. The Rust
  predicate now lives in `ff-core::caps` so `ff-backend-valkey` and the
  future `ff-backend-postgres` share one definition. New public surface:
  `ff_core::caps::{matches, matches_csv, CapabilityRequirement}`.
  `ff-scheduler` now calls `ff_core::caps::matches_csv` for the fast-path
  short-circuit; the Lua `ff_issue_claim_grant` FCALL remains the atomic
  authority for Valkey. **No behavior change**: `matches_csv` is the
  line-for-line replacement of the old private helper (same case-sensitive
  subset semantics, same empty-required-matches-any default). Token
  validation and `CAPS_MAX_{BYTES,TOKENS}` bounds stay at the existing
  ingress sites. Ref: RFC-v0.7 §Q7, PR #230 (Wave 0 scaffold).
- **RFC-v0.7 Wave 1c: `Handle.opaque` codec moved to
  `ff_core::handle_codec` with embedded `BackendTag`.** The
  byte-layout / wire-version logic that previously lived at
  `ff_backend_valkey::handle_codec` is now a public module in
  `ff_core` so the future Postgres backend decodes the same shape.
  New wire format v2 prefixes the buffer with a `0x02` magic byte +
  one-byte wire version + one-byte `BackendTag` so cross-backend
  migration tooling can detect which backend minted a given handle
  without parsing the payload. Pre-Wave-1c (v1) Valkey handles still
  decode cleanly under `BackendTag::Valkey` via a compat path keyed
  off the legacy leading byte `0x01` — backward wire compat is
  non-negotiable per the master spec. New public types:
  `ff_core::handle_codec::{HandlePayload, DecodedHandle, encode,
  decode, HandleDecodeError}`; new enum variant
  `BackendTag::Postgres`; new enum variant
  `ValidationKind::HandleFromOtherBackend` (returned by backends when
  a handle tagged for one backend is presented to another). No
  behaviour change for existing Valkey call sites — `ValkeyBackend`
  continues to produce and accept handles identically modulo the new
  leading byte, and the crate-internal `HandleFields` alias still
  names the same shape.

### Fixed

- **`ff-core` unit tests compile against `BackendConnection` enum added
  in Wave 0 (#230).** `backend_config_valkey_ctor` used an irrefutable
  `let` pattern that broke when the `Postgres` variant landed; switched
  to `let-else` with a descriptive panic. Pure test-only fix.

## [0.6.1] - 2026-04-24

### Fixed

- **RFC-015 `read_summary` RESP3 `Value::Map` decode (release-blocker
  regression from v0.6.0).** `ff-backend-valkey::read_summary_impl`
  matched only `Value::Array` for the `HGETALL` reply. ferriskey's
  default client on Valkey 7.2 negotiates RESP3, where `HGETALL`
  returns `Value::Map`, so every call to `EngineBackend::read_summary`
  returned `Ok(None)` regardless of whether a summary existed — breaking
  RFC-015 DurableSummary reads end-to-end. Writes and `summary_version`
  increments were unaffected. The decoder now accepts both
  `Value::Array` (RESP2) and `Value::Map` (RESP3) and normalizes into
  the same `HashMap<String, String>` before parsing into
  `SummaryDocument`. Caught by the v0.6.0 published-artifact smoke
  (PR #224). No wire-format, trait, or Lua changes.

## [0.6.0] - 2026-04-23

### Changed

- **RFC-015 §4.2: dynamic MAXLEN sizing for `BestEffortLive`.** The
  shipped `BestEffortLive` mode trimmed every append at a static
  `MAXLEN ~ 64` (RFC §4.1 round-2 default). Phase 0 measurement showed
  this retained ~1.3 s of history on a bursty LLM-token workload — only
  0.6 % of frames visible inside a 30 s TTL. This change implements the
  RFC §4.2 EMA path: each best-effort `XADD` now derives `K` from
  `ceil(ema_rate_hz * ttl_ms / 1000) * 2` clamped to `[floor, ceiling]`
  with the EMA computed in Lua and persisted on the per-attempt
  `stream_meta` Hash (`ema_rate_hz`, `last_append_ts_ms`,
  `maxlen_applied_last`). Defaults: **α = 0.2**, **floor = 64**,
  **ceiling = 16_384** (raised from the RFC draft's 2048 — all α
  variants saturated the lower clamp on the target workload). New
  `BestEffortLiveConfig` on `ff_core::backend` exposes all four knobs
  with builder-style setters; `StreamMode::best_effort_live(ttl_ms)`
  still uses defaults for the common case, and new
  `StreamMode::best_effort_live_with_config(cfg)` is available for
  tuning. Wire-level: `ff_append_frame` ARGV extended from 16 to 19
  (backwards-compatible — Lua defaults missing ARGV to the RFC-final
  values). Closes RFC-015 §4.3. Lua bundle version bumped 24 → 25.
  Re-validated via `benches/harness/src/bin/ema_alpha_bench.rs`:
  visibility at α=0.2 under the same Phase 0 workload climbs from
  0.6 % (static-64) to 98.3 %.

- **RFC-016 Stage E: full integration test matrix + cluster co-location.**
  Test-only PR (no Lua, no impl code, no wire-format changes). Adds
  `crates/ff-test/tests/flow_edge_policies_stage_e.rs` (11 new tests;
  RFC-016 coverage is now 28 integration tests across Stages A–E):
  the four-way `(AnyOf|Quorum) × (CancelRemaining|LetRun)` cancel-to-
  terminal matrix with per-sibling `cancellation_reason` assertions;
  cluster co-location hash-tag pin (flow_core + edgegroup hash +
  `pending_cancel_groups` SET + every member exec_core share one
  `{fp:N}` slot, validating single-slot operation under cluster mode);
  skip-propagation cascade across two edge layers (impossible-quorum
  at layer 1 cascades to AllOf skip at layer 2); one-shot replay
  idempotence (re-resolving a winning upstream after the dispatcher
  terminated siblings must not re-fire the downstream or advance
  counters — edge-id dedup + HSETNX `satisfied_at`); a real OTEL
  registry threaded through `Server::start_with_metrics` backing
  `ff_sibling_cancel_dispatched_total{reason}` + `ff_sibling_cancel_
  disposition_total{disposition}` pre/post assertions; idempotent-drain
  stress at 8 concurrent pending groups; correctness-only high-fanout
  at `Quorum(1, 50) + CancelRemaining` (Phase 0 benchmark at
  `n ∈ {10, 100, 1000}` remains the v0.6 release gate, NOT a Stage E
  deliverable); and a builder ergonomics round-trip
  (`EdgeDependencyPolicy::all_of()` / `any_of()` / `quorum(k, ...)` +
  `OnSatisfied::{cancel_remaining, let_run}()` + serde). `ff-test`
  now pulls `ff-server` + `ff-observability` with `observability` /
  `enabled` features through dev-deps so `Metrics::new()` backs real
  instruments during tests (dev-deps only; production callers
  unchanged). Stage E is the last Stage before Phase 0 high-fanout
  benchmarks gate v0.6 ship (RFC-016 §4.2).

### Added

- **RFC-016 Stage D: sibling-cancel reconciler (Invariant Q6 crash recovery).**
  New background scanner `edge_cancel_reconciler` (default interval 10s,
  env `FF_EDGE_CANCEL_RECONCILER_INTERVAL_S`) consumes the per-flow-
  partition `ff:idx:{fp:N}:pending_cancel_groups` SET directly (no full
  scan) and heals crash-recovery residue left when the Stage C
  dispatcher was interrupted between `SADD` + `ff_drain_sibling_cancel_
  group`. New Lua function `ff_reconcile_sibling_cancel_group` decides
  atomically per tuple: SREM stale entries (flag cleared / edgegroup
  missing), complete interrupted drains (flag true + all siblings
  terminal ⇒ HDEL flag + members + SREM), or no-op when siblings are
  still non-terminal (dispatcher's domain). The reconciler MUST NOT
  fight the dispatcher — it never re-attempts cancel dispatch. New
  metric `ff_sibling_cancel_reconcile_total{action}` with cardinality 3
  (`sremmed_stale` | `completed_drain` | `no_op`), no per-flow/exec
  labels. New `EngineConfig::edge_cancel_reconciler_interval` (default
  `Duration::from_secs(10)`). Lua library version 23 → 24. Stage E
  (full cancel-to-terminal matrix + stress tests) + Phase 0 benchmarks
  (v0.6 release gate) remain outstanding.

- **RFC-014: Multi-signal resume conditions (`AllOf`, `Count{DistinctX}`).**
  `ResumeCondition::Composite(CompositeBody)` — the RFC-013 placeholder
  — is now populated with two variants: `CompositeBody::AllOf { members:
  Vec<ResumeCondition> }` (all sub-conditions must be satisfied) and
  `CompositeBody::Count { n, count_kind, matcher, waitpoints }` with
  `CountKind ∈ { DistinctWaitpoints, DistinctSignals, DistinctSources }`.
  Rust-side validation enforces RFC-014 §5.1 invariants at suspend-time
  (depth cap 4 via `ff_core::contracts::MAX_COMPOSITE_DEPTH`, `n > 0`,
  non-empty `waitpoints`, `n ≤ waitpoints.len()` for DistinctWaitpoints,
  non-empty `AllOf.members`); failures surface as
  `EngineError::Validation { kind: InvalidInput, detail: "..." }` per
  §5.1.1. The Valkey backend serializes composites via a new tagged-tree
  wire format (`{ v: 1, composite: true, tree: ... }`, §7.2), and the
  Lua evaluator (`lua/helpers.lua` + `lua/signal.lua`) walks the tree
  depth-boundedly per signal, using `ff:exec:{p:N}:<eid>:suspension:
  current:satisfied_set` (SET, RFC-014 §3.1) for durable satisfier
  tokens and a `:member_map` HASH for operator diagnostics. Satisfier
  tokens: `wp:<id>` / `sig:<id>` / `src:<type>:<identity>` +
  `leaf:<path>` / `node:<path>` per §3.2. Matcher-filter step
  (`signal_ignored_matcher_failed`) lands pre-SADD; SADD gives
  idempotency (`appended_to_waitpoint_duplicate`). The closer signal id
  + full satisfier set are published on `suspension:current` as
  `closer_signal_id` + `all_satisfier_signals` (JSON array) per §4.5;
  `resume_signals()` / `observe_signals` now reads this composite path
  ahead of the legacy matcher-array shape. `ff_cancel_execution`,
  `ff_expire_suspension`, and `ff_resume_execution` all delete the
  composite state keys on the three terminating paths (§3.1.1). SDK
  surface: `CompositeBody` + `CountKind` re-exported at
  `ff_sdk::{CompositeBody, CountKind}` and `ff_sdk::task::{CompositeBody,
  CountKind}`; `ClaimedTask::try_suspend_inner` rebinds the Fresh
  waitpoint_key from a composite tree's first Single/Count key so
  composite scoping works end-to-end for patterns 1 + 2, and RFC-014
  Pattern 3 (heterogeneous subsystems across N distinct waitpoint_ids,
  widened `SuspendArgs.waitpoints: Vec<WaitpointBinding>`) also ships
  in this release (see Changed below). Lua library version bumped
  20 → 22 (21 → 22 covers the Pattern 3 multi-waitpoint widening).

- **RFC-016 Stage C: sibling-cancel dispatcher + `pending_cancel_groups`
  index SET.** Builds on Stage B's `cancel_siblings_pending_flag` to
  land the end-to-end "kill the losers" path. `ff_resolve_dependency`
  gains two new KEYS (`incoming_set`, `pending_cancel_groups_set`) and
  two new ARGV (`flow_id`, `downstream_eid`). On the terminal
  CancelRemaining transition the Lua resolver now ALSO (a) enumerates
  still-running siblings from the downstream's `incoming_set` + each
  sibling's `exec_core.lifecycle_phase`, (b) writes the pipe-delimited
  sibling execution-id list to `cancel_siblings_pending_members` on
  the edgegroup hash, and (c) SADDs the `<flow_id>|<downstream_eid>`
  tuple to `ff:idx:{fp:N}:pending_cancel_groups`. New scanner
  `ff-engine::scanner::edge_cancel_dispatcher` SRANDMEMBERs the
  pending SET per flow partition, reads each group's members + reason
  from the edgegroup hash, issues per-sibling `ff_cancel_execution`
  with `cancellation_reason = sibling_quorum_{satisfied,impossible}`,
  tracks per-id dispositions (`cancelled` | `already_terminal` |
  `not_found`), and finally atomically clears the flag + members +
  SET tuple via a new `ff_drain_sibling_cancel_group` Lua function —
  one atomic unit so a crash mid-drain leaves either pre- or post-
  drain state, never a torn in-between. The dispatcher reuses the
  existing `ff_cancel_execution` FCALL (no new cancel variant); the
  reason code is threaded through ARGV[2] and lands verbatim on
  exec_core's `cancellation_reason`. `LetRun` is structurally
  excluded — the Lua resolver never flags / enumerates / indexes
  LetRun groups, so the dispatcher can never see them.
  `EngineConfig::edge_cancel_dispatcher_interval` (default 1s, env
  override `FF_EDGE_CANCEL_DISPATCHER_INTERVAL_S`) controls the scan
  cadence. New metrics: `ff_sibling_cancel_dispatched_total{reason}`
  (2 labels) and `ff_sibling_cancel_disposition_total{disposition}`
  (fixed cardinality = 3). Integration coverage:
  `crates/ff-test/tests/flow_edge_policies_stage_c.rs` — 4 scenarios
  (AnyOf{CancelRemaining} fan-out-3, Quorum(3/5)+CancelRemaining
  stragglers, impossible-quorum survivors, AnyOf{LetRun}
  regression). Benchmark harness landed at
  `benches/harness/benches/quorum_cancel_fanout.rs` per RFC-016 §4.2
  — runs pre-release, SLO is p99 ≤ 500 ms at `n=100` under
  `Quorum(1, n) + CancelRemaining` (no enforcement at commit time).
  **Stage C does NOT ship the crash-mid-cancel reconciler (Stage D),
  the full cancel-path integration matrix (Stage E), or the benchmark
  execution / SLO enforcement (release gate).** Lua library bumped
  22 → 23.
- **RFC-016 Stage B: AnyOf / Quorum edge-dependency resolver.** Lights
  up the four-counter state machine on top of the Stage A edgegroup
  hash. `EngineBackend::set_edge_group_policy` now ACCEPTS
  `EdgeDependencyPolicy::AnyOf { on_satisfied }` and
  `EdgeDependencyPolicy::Quorum { k, on_satisfied }` (Stage A rejected
  them with a typed validation error); only `k == 0` / absurd `k` and
  unknown `#[non_exhaustive]` variants remain rejected. The Lua
  `ff_resolve_dependency` function gains a quorum branch that, when the
  edgegroup hash's `policy_variant` is `any_of` or `quorum`, owns the
  downstream transition via the §3 four-counter model: `succeeded`,
  `failed`, `skipped` against frozen `n`. `AnyOf` fires on first
  success; `Quorum(k)` fires on the k-th success; either short-circuits
  to `impossible` (and skip-propagates the downstream) as soon as
  `failed + skipped > n - k`. Transitions are strictly one-shot
  (Invariant Q1) — late upstream terminals advance counters for
  telemetry without re-firing or re-skipping. `OnSatisfied::
  CancelRemaining` writes a `cancel_siblings_pending_flag = true`
  (plus `cancel_siblings_reason` = `sibling_quorum_satisfied` or
  `sibling_quorum_impossible`) into the edgegroup hash on the terminal
  transition; `OnSatisfied::LetRun` NEVER writes the flag, regardless
  of whether the group terminates via `satisfied` or `impossible`
  (RFC-016 §5, adjudication 2026-04-23 — `LetRun` is pure).
  **Stage B does NOT ship the sibling-cancel dispatcher, the
  `ff:pending_cancel_groups:{p:N}` index SET, the reconciler, or
  full cancel-to-terminal integration coverage — those land in
  Stages C / D / E.** `ff_edge_group_policy_total{policy}` now emits
  all three labels (`all_of` / `any_of` / `quorum`). The Stage A
  snapshot decoder (`describe_flow.edge_groups`) was extended to
  surface `AnyOf { on_satisfied }` and `Quorum { k, on_satisfied }`
  from the edgegroup hash. Lua `ff_set_edge_group_policy` ARGV widens
  from 3 → 4 (adds `k`). Integration coverage:
  `crates/ff-test/tests/flow_edge_policies_stage_b.rs` (8 scenarios:
  AnyOf × {CancelRemaining, LetRun}, Quorum(3/5) × {CancelRemaining,
  LetRun}, impossible-quorum × {CancelRemaining, LetRun}, `k=0`
  rejection, once-fired semantics). The retired Stage-A-only
  `stage_a_rejects_any_of_and_quorum` assertion is replaced by a
  module-level doc stub pointing at the Stage B file. Lua library
  bumped 19 → 20.
- **RFC-015: Stream durability modes (`DurableSummary`, `BestEffortLive`).**
  `append_frame` now carries a per-call `StreamMode` (on the `Frame`
  value) selecting one of `Durable` (default, pre-015 behaviour),
  `DurableSummary { patch_kind: PatchKind }`, or
  `BestEffortLive { ttl_ms: u32 }`. `DurableSummary` frames apply a
  JSON Merge Patch (RFC 7396) to a server-side rolling summary Hash at
  `ff:attempt:{p:N}:<eid>:<aidx>:summary` inside a single Valkey
  Function invocation (atomic decode → merge → encode → version bump)
  and also XADD the delta with `mode=summary summary_version=<n>`
  fields so live tailers continue to observe change events.
  `BestEffortLive` frames XADD with `mode=best_effort`, trim to a
  conservative `MAXLEN ~ 64` (EMA-driven sizing is benchmark-gated per
  RFC-015 §4.3 and ships post-measurement), and set `PEXPIRE` on the
  stream key *only* while the stream has never held a durable frame —
  the Lua function `PERSIST`s the key on the first durable append and
  never re-sets a TTL thereafter (mixed-mode safety, §4.1).
  `AppendFrameOutcome` is now `#[non_exhaustive]` and gains
  `summary_version: Option<u64>` populated on `DurableSummary` appends
  (§9). `EngineBackend::tail_stream` takes a new `TailVisibility` arg
  (`All` default / `ExcludeBestEffort`) that filters XADD entries by
  their `mode` field server-side (§6.1), and a new
  `EngineBackend::read_summary` returns the materialised
  `SummaryDocument` for an attempt (§6.3). The RFC-7396 "null means
  delete" ambiguity is handled by a byte-exact sentinel string
  `"__ff_null__"` (exposed as `ff_core::backend::SUMMARY_NULL_SENTINEL`
  / `ff_sdk::SUMMARY_NULL_SENTINEL`) which the Lua applier rewrites to
  JSON null post-merge; the round-trip invariant (the sentinel never
  appears in a returned `SummaryDocument`) is pinned at the type level
  and in integration tests. SDK surface:
  `ClaimedTask::append_frame_with_mode`,
  `ClaimedTask::tail_stream_with_visibility`, free-function
  `tail_stream_with_visibility`, re-exports for `StreamMode`,
  `TailVisibility`, `SummaryDocument`, `PatchKind`, and
  `SUMMARY_NULL_SENTINEL` under `ff_sdk`. Integration coverage:
  `crates/ff-test/tests/engine_backend_stream_modes.rs` (9 scenarios).
  No behaviour change for pre-015 callers — `Frame::new` defaults
  `mode` to `Durable`, `tail_stream` defaults `visibility` to `All`,
  pre-015 XADD entries without a `mode` field are read as `durable`
  per §8.1. Owner-adjudicated decisions honoured: `KeepAllDeltas`
  dropped; `JsonMergePatch` the sole v0.6 `PatchKind` (open enum for
  the future `StringAppend`); EMA α deferred-to-measurement, not baked
  in. Lua library bumped to version `17`.
- **RFC-016 Stage A: edge-dependency-policy foundations.** Introduces
  the `EdgeDependencyPolicy` (`AllOf` / `AnyOf` / `Quorum`) and
  `OnSatisfied` (`CancelRemaining` / `LetRun`) enums — both
  `#[non_exhaustive]` with constructor helpers — plus a new
  `EngineBackend::set_edge_group_policy(flow_id, downstream_eid,
  policy)` op backed by Lua `ff_set_edge_group_policy`. Stage A
  honours only `AllOf`; `AnyOf` / `Quorum` are rejected with
  `EngineError::Validation { kind: InvalidInput, .. }` until Stage B
  (RFC-016 §11) lands the resolver extension. `FlowFabricWorker::
  set_edge_group_policy` is the consumer entry point.

  A new per-downstream hash key
  `ff:flow:{fp:N}:<flow_id>:edgegroup:<downstream_eid>` carries the
  Stage A counters (`policy_variant`, `n`, `succeeded`,
  `group_state`). `ff_apply_dependency_to_child` dual-writes to the
  new hash on every edge application; `ff_resolve_dependency`
  dual-writes the success / impossibility counters. The existing
  `deps_meta.unsatisfied_required_count` counter remains the
  authoritative eligibility driver for Stage A (zero behaviour change
  on existing flows); the edgegroup hash is the new source of truth
  for the `EdgeGroupSnapshot` surface and the foundation Stage B's
  resolver extends.

  **Migration.** Flows created before Stage A never populated the
  edgegroup hash. `describe_flow.edge_groups` falls back to decoding
  from `deps_meta.unsatisfied_required_count` /
  `impossible_required_count` when the hash is absent — the snapshot
  still reports `AllOf` + correct counters, observability is
  transparent to operators. Future stages (Stage C `pending_cancel_
  groups` reconciler + Stage E observability pagination) will retire
  the shim once every live flow has transitioned; the shim is
  tracked for removal post-Stage E.

  Wire addition: `FlowSnapshot` gains `edge_groups:
  Vec<EdgeGroupSnapshot>` (`#[non_exhaustive]`). Metric addition:
  `ff_edge_group_policy_total{policy}` counter on `ff-observability`
  — Stage A emits only `policy="all_of"`; the `any_of` / `quorum`
  labels activate in Stage B.

  Stage B/C/D/E scope (NOT in this PR): the `AnyOf` / `Quorum`
  resolver (Stage B), `CancelRemaining` sibling-cancel dispatcher +
  reconciler + `pending_cancel_groups` SET (Stage C), `LetRun` path
  (Stage D), operator tooling + pagination (Stage E).

  Lua library version 17 → 18 (new `ff_set_edge_group_policy`
  function; `ff_apply_dependency_to_child` KEYS 7 → 8;
  `ff_resolve_dependency` KEYS 11 → 12). Integration test:
  `crates/ff-test/tests/flow_edge_policies_stage_a.rs`. Existing
  readiness suite (`e2e_flow_atomicity`, `fanout_fanin_flow_*`,
  `e2e_lifecycle::test_flow_*`) unaffected.

### Changed

- **RFC-014 Pattern 3 — `SuspendArgs.waitpoint` widened to
  `waitpoints: Vec<WaitpointBinding>` (pre-1.0 breaking change).**
  Heterogeneous-subsystems composites (`AllOf` across N distinct
  waitpoint_ids — "db-migration-complete AND cache-warmed AND
  feature-flag-set") now land in the same suspension trait surface
  PR #217 scoped to single-waitpoint. `SuspendArgs::new(..)` still
  takes one primary `WaitpointBinding`; append further bindings via
  `SuspendArgs::with_waitpoint(binding)` (appends, pre-1.0
  incremental-migration friendly) or replace the whole vector via
  `SuspendArgs::with_waitpoints(vec)`. Call `args.primary()` for the
  first binding.

  `SuspendOutcomeDetails` gains an `additional_waitpoints:
  Vec<AdditionalWaitpointBinding>` field carrying per-extra
  `(waitpoint_id, waitpoint_key, waitpoint_token)`; the top-level
  `waitpoint_id` / `waitpoint_key` / `waitpoint_token` continue to
  identify the primary (`waitpoints[0]`).

  Valkey backend `ff_suspend_execution` now accepts
  `KEYS = 18 + 3*N_extra` + `ARGV = 19 + 1 + 2*N_extra`; for each
  extra binding Lua mints an independent HMAC token bound to its own
  `ff:wp:{tag}:<id>` hash + `ff:wp:{tag}:<id>:condition` hash so
  external signallers can deliver to any of the N waitpoints. The
  full set is recorded on `suspension:current.additional_waitpoints_json`
  so cleanup owners (`ff_cancel_execution`, `ff_expire_suspension`,
  `ff_resume_execution`, composite-resume close in `ff_deliver_signal`)
  iterate every per-extra waitpoint when closing the suspension.

  SDK builder: `ResumeCondition::all_of_waitpoints([wp_a, wp_b, wp_c])`
  (RFC-014 §10.3) is the canonical Pattern 3 shorthand, desugaring to
  `AllOf { members: vec![Single{Wildcard}, Single{Wildcard}, ...] }`.
  For per-member matchers construct the tree directly.

  **Idempotency-key dedup widening (RFC-013 × RFC-014).** The
  serialized dedup outcome adds an `N_extra` count + `N_extra × (id,
  key, tok)` tail; replaying the same `idempotency_key` within the
  TTL returns the full set verbatim (primary + extras). Tab-separated
  wire format, always present even when `N_extra = 0`.

  **Validator widenings.** `validate_suspend_args` now rejects
  `waitpoints_empty` (defensive; `SuspendArgs::new` guarantees ≥1);
  every additional binding must be `WaitpointBinding::Fresh` with a
  non-empty `waitpoint_key` (UsePending + extras is rejected —
  buffered-signal activation is inherently single-waitpoint); and for
  multi-binding suspends the condition's referenced waitpoint_keys
  must be exactly the binding set (`referenced_waitpoint_key_missing_binding`,
  `extra_binding_not_referenced_by_condition`).

  Lua library version bumped 21 → 22.

- **`ff-script` gates its `ferriskey` dep behind a new `valkey-client`
  feature (#171).** Closes the backend-agnosticism leak flagged in the
  feature-flag-propagation memory: `ff-sdk --no-default-features` used
  to still pull `ferriskey` through `ff-sdk → ff-script → ferriskey`
  (the `ff-script` edge was unconditional). `ff-script`'s default
  feature set is now **empty**; every internal consumer
  (`ff-sdk[valkey-default]`, `ff-backend-valkey`, `ff-server`,
  `ff-scheduler`, `ff-test`) explicitly enables
  `ff-script/valkey-client`. Default workspace builds and
  `--all-features` are unchanged; `cargo tree -p ff-sdk
  --no-default-features` now shows a `ferriskey`-free graph.

  **Migration for external consumers that depend on `ff-script`
  directly:** add `features = ["valkey-client"]` to your `ff-script`
  Cargo entry if you reference any of:
  `ScriptError::Valkey`, `ScriptError::valkey_kind`,
  `ff_script::engine_error_ext::valkey_kind`,
  `ff_script::loader`, `ff_script::retry`, `ff_script::result`,
  `ff_script::functions`, `ff_script::stream_tail`,
  `ff_script::macros`, `FromFcallResult`, `is_retryable_kind`, or
  `kind_to_stable_str`. Consumers using only the Lua-error enum
  variants, `ScriptError::class()`, `From<ScriptError> for
  EngineError`, `engine_error_ext::{class, transport_script,
  transport_script_ref}`, or the `LIBRARY_SOURCE` / `LIBRARY_VERSION`
  constants need no change. No public item was removed or renamed —
  only cfg-gated.

### Fixed

- **`tail_stream` no longer head-of-line-blocks concurrent writes (#204).**
  `ValkeyBackend::tail_stream` used to issue `XREAD BLOCK` on the shared
  ferriskey multiplex, so a concurrent `append_frame`/`XADD` on the same
  `ClaimedTask` would wait behind the blocking read — or, more commonly,
  be misreported to the caller as `EngineError::Transport { source:
  "timed out" }` when ferriskey's client request-timeout fired first.
  `tail_stream_impl` now obtains a dedicated connection via a new
  `ferriskey::Client::duplicate_connection()` method, runs the BLOCK on
  that socket, and drops it on return. Writes on the shared client are
  unaffected by blocking reads. Regression test:
  `crates/ff-test/tests/issue_204_tail_stream_no_hol.rs`.

- **`list_lanes` populated from `ServerConfig.lanes` at boot (#203).**
  `EngineBackend::list_lanes` reads `SMEMBERS ff:idx:lanes`. Before
  this fix no code path wrote to that SET. `Server::start` now SADDs
  every `ServerConfig.lanes` entry at boot. `ff:idx:lanes` is a
  non-hash-tagged global SET, so dynamic lane registration via the
  partition-locked `ff_create_execution` Lua path was attempted in
  PR #207 but reverted — FCALL can only touch declared KEYS, and a
  cross-partition SET violates the cluster single-slot invariant.
  Consumers needing dynamic lanes must extend `ServerConfig.lanes`
  before boot. A future RFC may introduce per-partition lane
  registries if cluster-safe dynamic registration becomes a
  requirement.

- **HTTP-routed claim now re-claims resumed executions (#150, Bug B).**
  `FlowFabricWorker::claim_from_grant` (the entry used by
  `claim_via_server`) missed the `UseClaimResumedExecution` contention
  fallback that `claim_next` already performed, so scheduler-routed
  callers saw raw `SdkError::Engine(Contention(UseClaimResumedExecution))`
  when picking up an `attempt_interrupted` execution after a
  suspend→resume. The fallback now mirrors `claim_next` and transparently
  forwards to `claim_resumed_execution`, matching the direct-claim path.

### Added

- **`EngineBackend::suspend` is a typed trait method (RFC-013 Stage 1d,
  #117).** The round-4 stub at `EngineError::Unavailable { op: "suspend" }`
  is replaced by a real Valkey impl. The signature moves from
  `(&Handle, Vec<WaitpointSpec>, Option<Duration>) -> Result<Handle, _>`
  to `(&Handle, SuspendArgs) -> Result<SuspendOutcome, _>`, closing
  RFC-012 §R7.6.1. The typed `SuspendArgs` carries a first-class
  `ResumeCondition` (RFC-014 extends the `Composite` slot without
  breaking this shape), `ResumePolicy`, `TimeoutBehavior`,
  `SuspensionReasonCode`, `SuspensionRequester`, and optional
  `IdempotencyKey` for retry-safe replay. The typed `SuspendOutcome`
  encodes the `Suspended` / `AlreadySatisfied` split as an enum of
  structs so the lease-released vs lease-retained branch is pattern-
  matchable at the call site rather than gated on a nullable handle.
  New public types in `ff_core::contracts` (re-exported through
  `ff_core::backend` and `ff_sdk`): `SuspendArgs`, `SuspendOutcome`,
  `SuspendOutcomeDetails`, `WaitpointBinding`, `ResumeCondition`,
  `CompositeBody` (empty placeholder RFC-014 populates), `ResumePolicy`,
  `ResumeTarget`, `TimeoutBehavior`, `SuspensionReasonCode`,
  `SuspensionRequester`, `SignalMatcher`, `IdempotencyKey`. New
  `StateKind::AlreadySatisfied` variant surfaces the strict-path
  refusal of the early-satisfied branch. `ClaimedTask::suspend` is now
  the strict wrapper returning `SuspendedHandle`; new
  `ClaimedTask::try_suspend` + `ClaimedTask::try_suspend_on_pending`
  return `TrySuspendOutcome`, retaining the `ClaimedTask` on
  `AlreadySatisfied`. The SDK-internal `parse_suspend_result`,
  `ConditionMatcher`, and SDK-local `TimeoutBehavior` / `SuspendOutcome`
  types are removed; the wire contract is parsed in the backend and
  the typed surface re-exported from `ff_core`. Lua
  `ff_suspend_execution` grows one KEY (dedup hash, slot 18) and two
  ARGV (`idempotency_key` slot 18, `dedup_ttl_ms` slot 19) for
  partition-scoped retry dedup; the non-dedup path is byte-for-byte
  unchanged. Library version bumped 16 → 17, forcing `FUNCTION LOAD
  REPLACE` on upgrade.

- **`EngineBackend::deliver_signal` + `EngineBackend::claim_resumed_execution`
  — trigger-surface trait methods (#150).** Two new core-feature-gated
  methods on `EngineBackend` that expose signal delivery and resumed-
  execution claim at the trait layer, so alternate backends (future
  Postgres) have an explicit obligation to honour them rather than
  forcing callers onto REST / direct-FCALL paths. Both take the
  existing `ff_core::contracts` typed args (`DeliverSignalArgs` →
  `DeliverSignalResult`; `ClaimResumedExecutionArgs` →
  `ClaimResumedExecutionResult`) and surface typed `ScriptError`
  failures (`invalid_token`, `not_a_resumed_execution`, etc.) via
  `EngineError::Transport`. The Valkey impl forwards through the
  existing `ff_script::functions::signal` FCALL wrappers. `ff-sdk`'s
  `FlowFabricWorker::deliver_signal` public API and the private
  `claim_resumed_execution` used by `claim_from_reclaim_grant` now
  route through the trait rather than firing `client.fcall` directly
  — closing the two `TODO(#150)` migration markers. `HookedBackend`
  and the layer-test `PassthroughBackend` grew matching dispatch
  arms. Integration coverage in
  `crates/ff-test/tests/engine_backend_{deliver_signal,claim_resumed}.rs`.

## [0.5.0] - 2026-04-23

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
