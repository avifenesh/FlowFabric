# Changelog

All notable changes to FlowFabric are documented here. Format loosely
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

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

### Added

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
