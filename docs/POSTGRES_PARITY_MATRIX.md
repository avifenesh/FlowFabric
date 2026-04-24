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
- **E2** — full `cancel_flow` trait migration (lifts `cancel_flow_inner`
  into `ValkeyBackend::cancel_flow_with_args`), `Server::client: Client`
  field removal, `fcall_with_reload` removal.
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

**Cross-cutting (unconditional, landed pre-Stage-A):**

| Method | Valkey | Postgres |
|---|---|---|
| `backend_label` | `"valkey"` | `"postgres"` |
| `shutdown_prepare` | `impl` (semaphore drain) | `impl` (ping check; pool drain a follow-up) |

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

### Remaining gaps for v0.8.0 (E1 honest-scope)

The Stage E1 smoke deliberately avoided these HTTP routes because the
server-side dispatch is still Valkey-bound:

- **`POST /v1/flows/{id}/cancel`** — `Server::cancel_flow_inner` still
  builds FCALL KEYS/ARGV and dispatches through `fcall_with_reload`;
  the idempotent-retry path does `HMGET` / `SMEMBERS` on `self.client`
  directly. **Stage E2** lifts `cancel_flow` onto the backend trait,
  removes `Server::client: Client`, and retires `fcall_with_reload`.
- **`GET /v1/executions/{id}` / `…/state` / `…/result`** — all three
  use `self.client.hgetall` / `hget` / `cmd("GET")` on the legacy
  Valkey client. Migrated as part of **Stage E2**'s field removal.
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
- **`Server::client: Client` on Postgres path** — the Stage E1
  Postgres branch still dials an ambient Valkey client for the
  residual legacy fields (`get_execution`, `get_execution_state`,
  `get_execution_result`, `fetch_waitpoint_token_v07`,
  `cancel_flow_inner`). That dial is not semantically meaningful for
  Postgres-only deployments; it exists only because the field type is
  `Client` not `Option<Client>`. **Stage E2** removes the field; the
  ambient dial goes with it.
