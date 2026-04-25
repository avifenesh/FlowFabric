# Postgres Parity Matrix — `EngineBackend` trait

**Source of truth** for per-method status across Valkey and Postgres
backends during the RFC-017 staged migration. Greppable by cairn-fabric
runbooks + operator tooling. Updated at every stage merge.

**Legend**

- `impl` — backend ships a real implementation (zero-behaviour-change
  wrapper or native path).
- `stub` — trait default `EngineError::Unavailable { op }` in use. Not
  a bug; a deliberate Stage marker. Every row that ships `stub` at
  **Stage D merge** is a hard block on the Stage D PR (RFC-017 §9.0
  L-1) — if the Postgres cell below is still `stub` by Stage D, the
  PR does not merge.
- `n/a` — method does not apply (e.g. streaming-feature-gated method
  on a backend that disables streaming).

**Fleet-wide cutover note (RFC-017 §9.0 round-2 K-R2-N1).** During
Stages B-D the `ff-server` binary hard-gates `FF_BACKEND=postgres` at
boot. Mixed-fleet rolling upgrades (some nodes on Stage-D binary,
some on Stage-E) with `FF_BACKEND=postgres` are unsupported. Operators
must complete a valkey→valkey Stage D→E rolling upgrade first and flip
`FF_BACKEND=postgres` as a second rollout.

**Stage E sub-split (owner-adjudicated 2026-04-24).** Stage E is
split into four PRs so each lands in a single session with CI-green:

- **E1** (`impl/017-stage-e1`) — Postgres dial branch in
  `Server::start_with_metrics` + `http_postgres_smoke.rs` test passing
  end-to-end for the 5 migrated ingress routes. `PostgresBackend::create_execution`
  trait impl added (inherent was there since Wave 4). `Server.engine`
  / `Server.scheduler` become `Option` (`None` on Postgres). Dev-mode
  override (`FF_BACKEND_ACCEPT_UNREADY=1 + FF_ENV=development`) still
  required; §9.0 hard-gate not yet flipped.
- **E2** (shipped, `impl/017-stage-e2`) — `cancel_flow` header + member-
  ack migrated through the backend trait via new provided-default
  methods (`cancel_flow_header`, `ack_cancel_member`), Valkey impls
  porting the Server bodies verbatim (plus `read_execution_info`,
  `read_execution_state`, `fetch_waitpoint_token_v07`);
  `Server::client: Client` field + Postgres-branch ambient Valkey
  dial + `fcall_with_reload` + `fcall_with_reload_on_client` +
  `parse_cancel_flow_raw` + `is_function_not_loaded` + `fcall_field_str`
  + standalone `ack_cancel_member` helper removed. Postgres path
  touches no Valkey at all.
- **E3** — `Server::claim_for_worker` trait cutover (Postgres-native
  scheduler), `Server::scheduler` field removal.
- **E4** — v0.8.0 cleanup: deprecated flat Valkey `ServerConfig` fields
  removed, legacy `waitpoint_token` wire field removed,
  `BACKEND_STAGE_READY` flipped to `&["valkey", "postgres"]`, version
  bump, cairn matrix published.

**Stage D split (owner-adjudicated 2026-04-24).** Stage D scope from
RFC-017 §9 was split into two PRs to keep reviewer cognitive load
bounded:

- **D1** (this row of flips) — §8 `PendingWaitpointInfo` schema
  rewrite, Valkey `list_pending_waitpoints` impl, 8 HTTP handler
  migrations, `Server::start_with_backend` entry point,
  `FF_BACKEND` env + §9.0 hard-gate wiring at stage `"D"`,
  deprecation audit log (`Deprecation: ff-017` header +
  `ff_pending_waitpoint_legacy_token_served_total`).
- **D2** (follow-up PR, required before v0.7 tag) — boot relocation
  into `ValkeyBackend::connect_with_metrics` (§4 row 12),
  `Server::client: Client` field removal (cascades to remaining
  handlers still using `fcall_with_reload`), `http_postgres_smoke.rs`
  integration test, Stage-B feature flag removal.

---

## RFC-017 Stage A trait surface (51 methods total)

### RFC-012 baseline (31 methods)

These landed with the initial trait in RFC-012 and are fully covered
on both backends. Table omitted for brevity; consult
`crates/ff-core/src/engine_backend.rs` pre-RFC-017-§5 for the full
list. Both backends tested across the hot path; streaming-feature
methods (`read_stream`, `tail_stream`, `read_summary`) are `impl` on
both when the `streaming` feature is enabled, `n/a` otherwise.

### RFC-017 Stage A additions (17 methods) — **this PR**

| # | Method | Valkey | Postgres | Notes |
|---|---|---|---|---|
| 1 | `create_execution` | `impl` | `impl` | **Stage E1 flip.** Valkey wraps `ff_create_execution` FCALL. Postgres trait impl lifts the existing inherent helper and always returns `CreateExecutionResult::Created { public_state: Waiting }` — the helper does not yet distinguish Created vs Duplicate; a follow-up may refine (`http_postgres_smoke` exercises it through HTTP). |
| 2 | `create_flow` | `impl` | `impl` | Promoted from PG inherent to trait. |
| 3 | `add_execution_to_flow` | `impl` | `impl` | Promoted from PG inherent to trait. |
| 4 | `stage_dependency_edge` | `impl` | `impl` | Promoted from PG inherent to trait. |
| 5 | `apply_dependency_to_child` | `impl` | `impl` | Promoted from PG inherent to trait. |
| 6 | `cancel_execution` | `impl` | `stub` | **Landed Stage C (PR #impl/017-stage-c).** Valkey body does HMGET pre-read (lane_id + current_attempt_index + current_waitpoint_id + current_worker_instance_id) then raw FCALL with variadic KEYS(21)/ARGV(5) matching `lua/execution.lua::ff_cancel_execution`. |
| 7 | `change_priority` | `impl` | `stub` | **Landed Stage C.** Valkey body reads authoritative lane via HGET when caller passes empty `lane_id`, then wraps the `ff_change_priority` ff-script helper. |
| 8 | `replay_execution` | `impl` | `stub` | **Landed Stage C.** Valkey body does HMGET pre-read (lane_id + flow_id + terminal_outcome). Non-flow path wraps `ff_replay_execution`; skipped-flow-member path does SMEMBERS over the flow partition's incoming-edge set and raw FCALL with variadic KEYS/ARGV (§4 row 3 Hard clause). |
| 9 | `revoke_lease` | `impl` | `stub` | **Landed Stage C.** Valkey body HGETs `current_worker_instance_id` when caller passes empty WIID; surfaces "no active lease" as `RevokeLeaseResult::AlreadySatisfied { reason: "no_active_lease" }` (Lua-canonical semantic — minor delta vs pre-migration HTTP 404, documented in Stage C PR). |
| 10 | `create_budget` | `impl` | `stub` | Valkey wraps `ff_create_budget`. Postgres default `Unavailable` until Wave 5 budget impls. |
| 11 | `reset_budget` | `impl` | `stub` | Valkey wraps `ff_reset_budget`. |
| 12 | `create_quota_policy` | `impl` | `stub` | Valkey wraps `ff_create_quota_policy`. |
| 13 | `get_budget_status` | `impl` | `stub` | **Landed Stage C.** Valkey body is 3× HGETALL (definition + usage + limits) + field-level parse, no FCALL. Missing budget surfaces as `EngineError::NotFound { entity: "budget" }` with contextual wrapper carrying the budget id. |
| 14 | `report_usage_admin` | `impl` | `stub` | Valkey wraps `ff_report_usage_and_check` without worker handle. |
| 15 | `get_execution_result` | `impl` | `stub` | Valkey direct `GET` of `ctx.result()`, binary-safe. |
| 16 | `list_pending_waitpoints` | `impl` | `stub` | **Landed Stage D1.** Pipelined SSCAN + 2× HMGET + §8 schema rewrite (HMAC redaction + `token_kid`/`token_fingerprint`) + `after`/`limit` pagination. HTTP handler wraps with the v0.7.x `Deprecation: ff-017` header and the raw `waitpoint_token` (fetched via the Valkey-only inherent `Server::fetch_waitpoint_token_v07`) for one-release deprecation warning. Postgres impl lands in Stage D2 (full parity gate). |
| 17 | `ping` | `impl` | `impl` | Valkey: `PING`. Postgres: `SELECT 1`. |
| 18 | `claim_for_worker` | `impl` | `stub` | **Landed Stage C.** `ff-backend-valkey` added `ff-scheduler` dep; `ValkeyBackend` holds `Option<Arc<ff_scheduler::Scheduler>>` wired at `ff-server` boot via `ValkeyBackend::with_scheduler` before the `Arc<dyn EngineBackend>` is sealed. Trait impl forwards to `Scheduler::claim_for_worker`; `SchedulerError::Config` maps to `EngineError::Validation`, Valkey transport errors via `transport_fk`. Backends without a wired scheduler (e.g. SDK-side `MockBackend`) surface `EngineError::Unavailable { op: "claim_for_worker (scheduler not wired on this ValkeyBackend)" }`. |
| 19 | `cancel_flow_header` | `impl` | `stub` | **Landed Stage E2.** Valkey body runs `ff_cancel_flow` FCALL with the caller-supplied reason + cancellation_policy + now + grace window, surfaces `flow_already_terminal` as `CancelFlowHeader::AlreadyTerminal` (with stored policy/reason + full SMEMBERS of members) so the Server can return an idempotent `Cancelled` without re-doing the flip. FCALL-with-reload (post-failover Lua re-install via `ff_script::loader::ensure_library`) inlined on the Valkey impl. Postgres returns `Unavailable` until Wave 9. |
| 20 | `ack_cancel_member` | `impl` | `stub` | **Landed Stage E2.** Valkey wraps `ff_ack_cancel_member` (SREM from `pending_cancels` + ZREM from `cancel_backlog` if empty). Called by `Server::cancel_flow_inner`'s sync + async dispatchers after each successful per-member cancel. Postgres returns `Unavailable` until Wave 9's cancel-backlog table write lands. |
| 21 | `read_execution_info` | `impl` | `stub` | **Landed Stage E2.** Valkey body is HGETALL on `exec_core` + field-by-field parse into the legacy `ExecutionInfo` wire shape (`StateVector` with all 7 sub-states), preserving HTTP response bytes. Returns `Ok(None)` when the hash is empty. Postgres returns `Unavailable` until the read-model migration lands. |
| 22 | `read_execution_state` | `impl` | `stub` | **Landed Stage E2.** Valkey body is HGET `public_state` on `exec_core` + JSON deserialize. Returns `Ok(None)` when the field is absent. Postgres returns `Unavailable`. |
| 23 | `fetch_waitpoint_token_v07` | `impl` | `stub` | **Landed Stage E2 (relocated from Server).** Valkey body is HGET `waitpoint_token` on the exec's waitpoint hash. Filters empty strings to `None`. Retires at v0.8.0 with the legacy wire field. Postgres returns `Unavailable`. |

**Cross-cutting (unconditional, landed pre-Stage-A):**

| Method | Valkey | Postgres |
|---|---|---|
| `backend_label` | `"valkey"` | `"postgres"` |
| `shutdown_prepare` | `impl` (semaphore drain) | `impl` (ping check; pool drain a follow-up) |
| `prepare` (issue #281) | `impl` (`FUNCTION LOAD` via `ff_script::loader::ensure_library`; returns `PrepareOutcome::Applied { description: "FUNCTION LOAD (flowfabric lib v<N>)" }`) | `impl` (returns `PrepareOutcome::NoOp` — schema migrations are applied out-of-band per v0.7 Wave 0 Q12; schema-version check runs at connect time) |

---

## Count reconciliation

Trait surface pre-RFC-017: **33 methods** (includes `backend_label` +
`shutdown_prepare` landed in Stage B pre-work).

- Note the RFC drafted "31 existing" but a direct `grep` on the
  pre-RFC trait found 33 methods. Both `backend_label` +
  `shutdown_prepare` were added in the Stage B pre-work landed on
  `main` via PR #264 before this Stage A backfill.

Trait surface after RFC-017 Stage A backfill: **50 methods** (33 + 17
new). The RFC's "51" target counts `backend_label` and
`shutdown_prepare` as Stage A additions; in-tree both landed early
(Stage B), so the net Stage A addition here is 17 methods — matching
the RFC's §2.3 breakdown minus the two cross-cutting methods already
on `main`.

Reporter must choose the canonical count: this PR uses **in-tree
count = 50**, matching `cargo expand` + `grep -cE 'async fn'` on the
trait. The RFC-cited "51" remains correct when counting the original
trait at 31 + the RFC §2.3 new-20; in-tree drift is due to
`backend_label` + `shutdown_prepare` landing earlier than the RFC
sequence anticipated.

---

## Stage boundaries

- **Stage A (this PR):** trait surface complete; Valkey impls for
  cheap single-FCALL ops; Postgres impls for ingress (promoted from
  inherent).
- **Stage B (shipped, PR #264):** read + admin + stream handler
  migration on Valkey.
- **Stage C (shipped, PR `impl/017-stage-c`):** operator control +
  budget-status + claim handler migration. The 6 Valkey `stub` rows
  above (cancel_execution, change_priority, replay_execution,
  revoke_lease, get_budget_status, claim_for_worker) moved to
  `impl`. `list_pending_waitpoints` remains `stub` pending Stage D's
  §8 schema rewrite. Stage C also migrated the 10 HTTP handlers (2
  operator control, 4 budget admin, 1 quota admin, 1 claim, 1 budget
  status, 1 admin `report_usage`) from inherent `Server::X(...)` +
  `fcall_with_reload` to `server.backend().X(...)` trait dispatch
  via the freshly minted `From<EngineError> for ApiError` bridge.
  The Engine `IntoResponse` arm gained `Conflict` / `Contention` /
  `State` kinds mapping to HTTP 409 per RFC-010 §10.7.
- **Stage D1 (this PR, `impl/017-stage-d1`):** §8 schema rewrite +
  Valkey `list_pending_waitpoints` impl + 8 HTTP handlers migrated
  to trait dispatch (`create_execution`, `create_flow`,
  `add_execution_to_flow`, `stage_dependency_edge`,
  `apply_dependency_to_child`, `get_execution_result`,
  `list_pending_waitpoints`, + header-only migration of
  `cancel_flow` — the async member dispatcher remains on `Server`
  pending D2). `Server::start_with_backend` entry point added;
  `FF_BACKEND` env + §9.0 hard-gate wired at stage `"D"` with the
  dev-mode override and `ff_backend_unready_boot_total{backend,stage}`
  metric. Deprecation audit log: `Deprecation: ff-017` response
  header + `ff_pending_waitpoint_legacy_token_served_total` counter
  + per-entry `pending_waitpoint_legacy_token_served` tracing event.
- **Stage D2 (follow-up, required before v0.7 tag):** boot
  relocation into `ValkeyBackend::connect_with_metrics` (§4 row 12),
  `Server::client: Client` field removal + cascading handler
  cutover, `http_postgres_smoke.rs` integration test, Stage-B
  feature flag removal (if present). **CI gate:**
  `test_postgres_parity_no_unavailable` asserts no Postgres
  `Unavailable` on any HTTP-exposed method. Every `stub` in the
  Postgres column above must be `impl` before Stage D2 merges.
- **Stage E1 (this PR, `impl/017-stage-e1`):** `Server::start_with_metrics`
  gains a Postgres branch — dials `PostgresBackend::connect_with_metrics`,
  runs `apply_migrations`, installs the backend as `Arc<dyn EngineBackend>`
  for all migrated HTTP ingress handlers. `Server::engine` and
  `Server::scheduler` become `Option<T>` (None on Postgres; Stage E3
  wires the Postgres-native scheduler). `PostgresBackend::create_execution`
  lifted onto the trait. New `crates/ff-test/tests/http_postgres_smoke.rs`
  (gated on `postgres-e2e` feature) passes end-to-end against a live
  Postgres for 5 migrated HTTP routes: create_flow, create_execution × 3,
  add_execution_to_flow × 3, stage_dependency_edge × 2, apply_dependency_to_child × 2.
- **Stage E (v0.8.0):** `BACKEND_STAGE_READY` updated to
  `&["valkey", "postgres"]`; `FF_BACKEND=postgres` boots successfully
  for the first time.

### Remaining gaps for v0.8.0 (E2 honest-scope)

Stage E2 closes the `Server::client` removal and lifts `cancel_flow`
+ `get_execution*` + `fetch_waitpoint_token_v07` onto the backend
trait (Valkey impls port the Server bodies verbatim; Postgres returns
`Unavailable` for rows that land in Wave 9). The Stage E1 smoke still
deliberately avoids the following HTTP routes because Postgres does
not yet implement the underlying trait method:

- **`POST /v1/flows/{id}/cancel`** — the Server side now dispatches
  through `backend.cancel_flow_header` + `backend.ack_cancel_member`;
  both methods have Valkey impls but Postgres returns `Unavailable`
  until Wave 9 lands the cancel machinery. Smoke remains Valkey-only.
- **`GET /v1/executions/{id}` / `…/state`** — dispatch through
  `backend.read_execution_info` / `backend.read_execution_state`;
  Valkey impls port the HGETALL / HGET + parse logic. Postgres
  default is `Unavailable` until the PG read-model lands.
- **`GET /v1/executions/{id}/result`** — trait method `get_execution_result`
  was already available; Valkey impl unchanged. Postgres returns
  `Unavailable` until the result-store migration lands.
- **`GET /v1/flows/{id}`** — this HTTP route does not exist today;
  callers infer flow state from member executions. Introducing it is
  an ingress-read task tracked post-E4.
- **`POST /v1/workers/{id}/claim`** — `Server::claim_for_worker` calls
  `self.scheduler.claim_for_worker` which wraps a `ferriskey::Client`.
  No Postgres equivalent yet; **Stage E3** wires the Postgres-native
  scheduler path.
- **Scheduler + engine scanners on Postgres** — the Postgres boot path
  skips `Engine::start_with_completions` (scanners hold a
  `ferriskey::Client`) and skips `Scheduler::with_metrics`. The
  Postgres reconciler helpers already live under
  `crates/ff-backend-postgres/src/reconcilers/` and the engine
  crate's `scan_tick_pg` wrappers exist; a Postgres-side scanner
  supervisor spawning them on an interval is a Stage E2/E3 task.
- **Dev-mode override** — `FF_BACKEND=postgres` still requires
  `FF_BACKEND_ACCEPT_UNREADY=1 + FF_ENV=development` through Stages E1-E3.
  Stage E4 flips `BACKEND_STAGE_READY` to include `"postgres"` and
  this requirement goes away.
- **`Server::client: Client` on Postgres path** — retired in
  **Stage E2**. The Postgres branch of `Server::start_with_metrics`
  no longer dials Valkey at all; the residual legacy-field paths all
  flow through the `EngineBackend` trait, and Postgres returns
  `Unavailable` for the methods whose impls land in Wave 9
  (`cancel_flow_header`, `ack_cancel_member`, `read_execution_info`,
  `read_execution_state`, `fetch_waitpoint_token_v07`).

---

## Release: v0.8.0 (Stage E4, 2026-04-24)

**Gate status:** `BACKEND_STAGE_READY = &["valkey", "postgres"]`.
`FF_BACKEND=postgres` boots natively; no `FF_BACKEND_ACCEPT_UNREADY`
dev-override. The §9.0 hard-gate still exists in `ff-server` as
defence-in-depth for future backend additions (e.g. SQLite, DynamoDB)
but no longer trips on Postgres.

### Parity summary at v0.8.0

| Family | Valkey | Postgres | Notes |
|---|---|---|---|
| Ingress (create_flow, create_execution, add_execution_to_flow, stage_dependency_edge, apply_dependency_to_child) | `impl` | `impl` | Full HTTP parity. `http_postgres_smoke` exercises all 5 end-to-end. |
| Flow family (`describe_flow`, `list_flows`, `list_edges`, `describe_edge`, `cancel_flow`, `set_edge_group_policy`) | `impl` | `impl` | RFC-v0.7 Wave 4c. |
| Flow cancel (`cancel_flow_header`, `ack_cancel_member`) | `impl` | `stub` | Wave 9 follow-up; Postgres `cancel_flow` covers the bulk path, the header/ack split lands with cancel-backlog machinery. |
| Read model (`read_execution_info`, `read_execution_state`, `get_execution_result`) | `impl` | `stub` | Wave 9 PG read-model migration. |
| Operator control (`cancel_execution`, `change_priority`, `replay_execution`, `revoke_lease`) | `impl` | `stub` | Wave 9. |
| Budget / quota (`create_budget`, `reset_budget`, `create_quota_policy`, `get_budget_status`, `report_usage_admin`) | `impl` | `stub` | Wave 9. |
| Scheduler (`claim_for_worker`) | `impl` | `impl` | Stage E3 `PostgresScheduler` + `claim_for_worker` trait impl + 6 reconcilers. |
| Waitpoints (`list_pending_waitpoints`) | `impl` | `stub` | Wave 9. |
| Admin rotation (`rotate_waitpoint_hmac_secret_all`) | `impl` | `stub` | Wave 9 (single-INSERT on the global HMAC table). |
| Admin seed (`seed_waitpoint_hmac_secret`, issue #280) | `impl` | `impl` | Idempotent boot-time seed so cairn can drop its raw HSET boot path. Valkey fans out per-partition HSET against the `waitpoint_hmac_secrets:{p:N}` layout. Postgres INSERTs one row into `ff_waitpoint_hmac` when no kid is active. `AlreadySeeded { same_secret }` lets callers distinguish replay from real conflict. |
| Cross-cutting (`backend_label`, `shutdown_prepare`, `ping`) | `impl` | `impl` | — |

### v0.8.0 shipped

- Stage E1 — Postgres HTTP ingress branch + `http_postgres_smoke`.
- Stage E2 — `Server::client` field retired on Postgres path;
  `cancel_flow_header` / `ack_cancel_member` / `read_execution_info` /
  `read_execution_state` / `fetch_waitpoint_token_v07` moved onto the
  trait (Valkey bodies ported verbatim; Postgres returns `Unavailable`
  for the Wave-9 rows).
- Stage E3 — `PostgresScheduler` + `claim_for_worker` trait impl +
  sibling-cancel, lease-timeout, completion-listener, cancel-backlog,
  flow-staging, edge-group-policy reconcilers.
- Stage E4a (this release) — `BACKEND_STAGE_READY` flip,
  `ServerConfig` flat-field removal, legacy `waitpoint_token` wire
  field removal, v0.8.0 version bump, CHANGELOG, migration doc.

### Deferred to v0.9 / post-0.8

Every Postgres column row still marked `stub` in the Stage A table
above lands in Wave 9 (RFC-018, scope TBD). No `stub` row is an
HTTP-exposed dispatch blocker at v0.8.0 because the corresponding
HTTP handlers return a structured `503 Unavailable` with an
`EngineError::Unavailable { op }` body — operators upgrading to
`FF_BACKEND=postgres` at v0.8.0 get a functional create/read/dispatch
+ scheduler loop and structured-error visibility on the missing
rows. The consumer migration guide (`docs/CONSUMER_MIGRATION_v0.8.md`)
lists the current Postgres `Unavailable` surface so consumers know
what to expect.
