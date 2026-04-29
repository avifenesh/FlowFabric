# Backend Parity Matrix — `EngineBackend` trait

> **Filename note (v0.12 / RFC-023 §9).** The file is named
> `POSTGRES_PARITY_MATRIX.md` for historical reasons; at v0.12 it
> gained a SQLite column and now documents all three backends. The
> filename is retained to avoid URL churn in external consumer
> references. Rename to `BACKEND_PARITY_MATRIX.md` is an open owner
> call if the cross-reference cost ever stops exceeding the
> clarity gain (RFC-023 §7, §9 owner note).

**RFC-018 Stage A note (2026-04-24, reshaped in v0.10):** this matrix
is now callable at runtime via
[`EngineBackend::capabilities()`](../crates/ff-core/src/engine_backend.rs)
— concrete `ValkeyBackend` / `PostgresBackend` impls populate a flat
`Capabilities { identity, supports }` value, and consumers (cairn
operator UI, operator tooling) dot-access the bools
(`backend.capabilities().supports.<field>`) to read the typed answer
at startup instead of parsing this file. v0.9 shipped a
`BTreeMap<Capability, CapabilityStatus>` shape; v0.10 reshaped to the
flat [`Supports`](../crates/ff-core/src/capability.rs) struct per
cairn's original #277 ask. This document remains the human-readable
reference during the RFC-017 migration; drift between the two is a
bug. Stage B (this file generated from the runtime value + CI drift
check) lands as a follow-up PR per RFC-018 §8.

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
| 5b | `resolve_dependency` | `impl` | `stub` | **PR-7b Step 0 overlap-resolver.** Valkey wraps `ff_resolve_dependency` FCALL (RFC-016 Stage C KEYS[14]+ARGV[5]). Postgres returns `Unavailable { op: "resolve_dependency" }`; PG's post-completion cascade runs via `ff_backend_postgres::dispatch::dispatch_completion(event_id)` keyed on the `ff_completion_event` outbox, not per-edge — the per-edge Valkey shape does not map cleanly. PG reconciler already calls `dispatch_completion` directly; PR-7b/final's integration test expects `Unsupported` from Valkey-shaped scanners on PG. SQLite mirrors PG. |
| 6 | `cancel_execution` | `impl` | `impl` | **Landed Stage C (Valkey) + Wave 9 v0.11 (Postgres, RFC-020).** Postgres wraps the §4.2 SERIALIZABLE-fn template + `ff_lease_event` emit. Valkey body does HMGET pre-read (lane_id + current_attempt_index + current_waitpoint_id + current_worker_instance_id) then raw FCALL with variadic KEYS(21)/ARGV(5) matching `lua/execution.lua::ff_cancel_execution`. |
| 7 | `change_priority` | `impl` | `impl` | **Landed Stage C (Valkey) + Wave 9 v0.11 (Postgres, RFC-020 §4.2.4).** Postgres UPDATE on `ff_exec_core` + `ff_operator_event` outbox emit (migration 0010); row-count=0 maps to `EngineError::ExecutionNotEligible` per Rev 7 Fork 3. Valkey reads authoritative lane via HGET when caller passes empty `lane_id`, then wraps `ff_change_priority`. |
| 8 | `replay_execution` | `impl` | `impl` | **Landed Stage C (Valkey) + Wave 9 v0.11 (Postgres, RFC-020 §4.2.5 Rev 7).** Postgres in-place UPDATE on existing `ff_attempt` (no `attempt_index` bump, matches Valkey) + `ff_edge_group` counter reset for skipped-flow-member path (Rev 7 Fork 1 Option A) + `ff_operator_event` emit. |
| 9 | `revoke_lease` | `impl` | `impl` | **Landed Stage C (Valkey) + Wave 9 v0.11 (Postgres, RFC-020).** Postgres SERIALIZABLE-fn path + `ff_lease_event` emit; surfaces "no active lease" as `RevokeLeaseResult::AlreadySatisfied { reason: "no_active_lease" }` matching Valkey. |
| 10 | `create_budget` | `impl` | `impl` | **Wave 9 v0.11 (RFC-020 §4.4, Rev 6).** Postgres INSERT on `ff_budget_policy` (migration 0013 adds scheduling + breach + definitional columns). Valkey wraps `ff_create_budget`. |
| 11 | `reset_budget` | `impl` | `impl` | **Wave 9 v0.11.** Postgres UPDATE clears breach counters + bumps `next_reset_at_ms`. Valkey wraps `ff_reset_budget`. |
| 12 | `create_quota_policy` | `impl` | `impl` | **Wave 9 v0.11 (RFC-020 §4.4, Rev 6).** Postgres INSERT on `ff_quota_policy` family (migration 0012: `ff_quota_policy` + `ff_quota_window` + `ff_quota_admitted`, 256-way HASH-partitioned on `partition_key`). Valkey wraps `ff_create_quota_policy`. |
| 13 | `get_budget_status` | `impl` | `impl` | **Wave 9 v0.11.** Postgres 2-table read on `ff_budget_policy` + `ff_budget_usage` with `hard_limits` / `soft_limits` parsed from `policy_json`. Valkey is 3× HGETALL. |
| 14 | `report_usage_admin` | `impl` | `impl` | **Wave 9 v0.11.** Postgres READ-COMMITTED INSERT-or-UPDATE on `ff_budget_usage` + incremental breach-counter bookkeeping on `ff_budget_policy` matching Valkey's pattern. |
| 15 | `get_execution_result` | `impl` | `impl` | **Wave 9 v0.11 (RFC-020 §4.1).** Postgres `SELECT result FROM ff_exec_core` (current-attempt semantics match Valkey's `GET ctx.result()`). |
| 16 | `list_pending_waitpoints` | `impl` | `impl` | **Landed Stage D1 (Valkey) + Wave 9 v0.11 (Postgres, RFC-020 §4.5).** Postgres `SELECT` against `ff_waitpoint_pending` with migration 0011 additive columns (`state`, `required_signal_names`, `activated_at_ms`); producer-side `suspend_ops` writes these on insert + activation. `waitpoint_key` was already shipped via migration 0004 (Rev 5 ground-truth correction). Valkey path: pipelined SSCAN + 2× HMGET. Both backends share the §8 D1 schema (HMAC redaction + `token_kid`/`token_fingerprint`) + `after`/`limit` pagination. |
| 17 | `ping` | `impl` | `impl` | Valkey: `PING`. Postgres: `SELECT 1`. |
| 18 | `claim_for_worker` | `impl` | `impl` | **Landed Stage C (Valkey) + Stage E3 (Postgres).** Valkey: `ff-backend-valkey` added `ff-scheduler` dep; `ValkeyBackend` holds `Option<Arc<ff_scheduler::Scheduler>>` wired at `ff-server` boot. Postgres: `PostgresScheduler` + 6 reconcilers shipped at Stage E3 (v0.8.0). Both backends report `Supports.claim_for_worker = true` when a scheduler is wired. |
| 19 | `cancel_flow_header` | `impl` | `impl` | **Landed Stage E2 (Valkey) + Wave 9 v0.11 (Postgres, RFC-020 §4.3).** Postgres UPDATE on `ff_flow` + insert into `ff_cancel_backlog` (migration 0014) driving the per-member cancel sweep; `AlreadyTerminal` returned idempotently with stored policy/reason on row-count=0. Valkey wraps `ff_cancel_flow` FCALL. |
| 20 | `ack_cancel_member` | `impl` | `impl` | **Landed Stage E2 (Valkey) + Wave 9 v0.11 (Postgres).** Postgres DELETE on `ff_cancel_backlog` member entry; drives the cancel-backlog reconciler. Valkey wraps `ff_ack_cancel_member` (SREM + conditional ZREM). |
| 21 | `read_execution_info` | `impl` | `impl` | **Landed Stage E2 (Valkey) + Wave 9 v0.11 (Postgres, RFC-020 §4.1).** Postgres multi-column projection on `ff_exec_core` + `LEFT JOIN LATERAL` on `ff_attempt` → `ExecutionInfo` / StateVector parse; partition-local. Returns `Ok(None)` when the row is absent. Valkey: HGETALL on `exec_core`. |
| 22 | `read_execution_state` | `impl` | `impl` | **Landed Stage E2 (Valkey) + Wave 9 v0.11 (Postgres, RFC-020 §4.1).** Postgres `SELECT public_state FROM ff_exec_core` (partition-local single-column point read) + JSON deserialize. Valkey: HGET `public_state` on `exec_core`. |
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
| Flow family (`describe_flow`, `list_flows`, `list_edges`, `describe_edge`, `cancel_flow`, `set_edge_group_policy`) | `impl` | `impl` | RFC-v0.7 Wave 4c. `cancel_flow` accepts all three `CancelFlowWait` modes (`NoWait`, `WaitTimeout(Duration)`, `WaitIndefinite`) on both backends via shared `wait_for_flow_cancellation` poll (#298). |
| Flow cancel (`cancel_flow_header`, `ack_cancel_member`) | `impl` | `stub` | Wave 9 follow-up; Postgres `cancel_flow` covers the bulk path, the header/ack split lands with cancel-backlog machinery. |
| Read model (`read_execution_info`, `read_execution_state`, `get_execution_result`) | `impl` | `stub` | Wave 9 PG read-model migration. |
| Operator control (`cancel_execution`, `change_priority`, `replay_execution`, `revoke_lease`) | `impl` | `stub` | Wave 9. |
| Budget / quota (`create_budget`, `reset_budget`, `create_quota_policy`, `get_budget_status`, `report_usage_admin`) | `impl` | `stub` | Wave 9. |
| Scheduler (`claim_for_worker`) | `impl` | `impl` | Stage E3 `PostgresScheduler` + `claim_for_worker` trait impl + 6 reconcilers. |
| Waitpoints (`list_pending_waitpoints`) | `impl` | `stub` | Wave 9. |
| Admin rotation (`rotate_waitpoint_hmac_secret_all`) | `impl` | `impl` | Shipped pre-v0.10; Postgres path is a single INSERT against `ff_waitpoint_hmac(kid, secret, rotated_at)` (ground truth: `crates/ff-backend-postgres/tests/capabilities.rs:44`). |
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

### RFC-019 Stage A — Stream-cursor subscriptions (4 methods)

| Method | Valkey | Postgres | Notes |
|---|---|---|---|
| `subscribe_lease_history` | `impl` | `impl` | Valkey: `duplicate_connection()` + `XREAD BLOCK 5000 STREAMS ff:part:{fp:N}:lease_history <cursor>`; cursor is `0x01 ++ ms(BE8) ++ seq(BE8)`. Postgres: `ff_lease_event` outbox + `LISTEN ff_lease_event`; cursor is `0x02 ++ event_id(BE8)`. Producer sites: `attempt::{claim,claim_from_reclaim,renew,complete,fail,delay,wait_children}`, `suspend_ops::{suspend_impl,claim_resumed_execution_impl}`, `flow::cancel_flow_once`, `reconcilers::{attempt_timeout,lease_expiry}`. **#282 ScannerFilter surface**: trait method takes `filter: &ScannerFilter`; Valkey gates via #122 `FilterGate` per-event HGET on `ff:exec:{p}:<eid>:tags`; Postgres filters inline against denormalised `namespace`/`instance_tag` columns on `ff_lease_event` (migration 0008). (Stage B, #308 / #282) |
| `subscribe_completion` | `impl` | `impl` | Postgres wraps `completion::subscribe` (`ff_completion_event` outbox + `LISTEN ff_completion`), durable via event-id cursor. Valkey (Stage B, #309) wraps the RESP3 `ff:dag:completions` pubsub subscriber — **partial: non-durable cursor (pubsub-backed, at-most-once over the live subscription window). Postgres impl is durable via outbox + cursor.** Durable Valkey completion subscription is a separate follow-up if demanded. **#282 ScannerFilter surface**: trait method takes `filter: &ScannerFilter`; non-noop filter routes both backends through the existing `subscribe_completions_filtered` path so multi-consumer isolation matches the #122 design. |
| `subscribe_signal_delivery` | `impl` | `impl` | Valkey: `duplicate_connection()` + `XREAD BLOCK 5000 STREAMS ff:part:{fp:N}:signal_delivery <cursor>`; cursor is `0x01 ++ ms(BE8) ++ seq(BE8)`. Producer XADD lives in `ff_deliver_signal` at KEYS[15]. Postgres: `ff_signal_event` outbox + `LISTEN ff_signal_event`; cursor is `0x02 ++ event_id(BE8)`. Producer INSERT lives in `suspend_ops::deliver_signal_impl`'s SERIALIZABLE tx. **#282 ScannerFilter surface**: trait method takes `filter: &ScannerFilter`; Valkey gates via the shared #122 `FilterGate`; Postgres filters in-memory against denormalised `namespace`/`instance_tag` columns on `ff_signal_event` (migration 0009). (Stage B, #310 / #282) |
| `subscribe_instance_tags` | `n/a` | `n/a` | Audited #311 (2026-04-24) + deferred: cairn's one-shot `instance_tag_backfill` pattern is served by `list_executions` + `ScannerFilter::with_instance_tag(..)` pagination; a realtime tag-churn stream is speculative demand we do not have today. Trait method remains and returns `Unavailable` on both backends; reserving the surface for future concrete demand. RFC-019 §instance_tags amended. |

### Post-v0.9 additive methods

| Method | Valkey | Postgres | Notes |
|---|---|---|---|
| `suspend_by_triple` | `impl` | `impl` | Cairn #322 service-layer entry point for suspend-by-triple (pause-by-operator, enter-waiting-approval, cancel-with-timeout-record). Valkey: HGETALL `exec_core` pre-read for `lane_id` / `current_attempt_index` / `current_worker_instance_id`, then the existing `ff_suspend_execution` FCALL with fence fields sourced from the triple — identical Lua dedup / §3 replay contract as `suspend`. Postgres: single `SELECT attempt_index FROM ff_exec_core` pre-read (Postgres attempts are keyed by `attempt_index`, not the triple's `attempt_id` — the `attempt_id` field is advisory on this backend), then the shared `suspend_core` SERIALIZABLE body. Default trait impl returns `EngineError::Unavailable { op: "suspend_by_triple" }` so downstream impls remain non-breaking. |

### Deferred to v0.9 / post-0.8 (historical — closed at v0.11.0)

At v0.8.0 every Wave-9 Postgres cell was `stub`. No `stub` row was
HTTP-exposed as a dispatch blocker because handlers returned a
structured `503 Unavailable` with an `EngineError::Unavailable { op }`
body. **All Wave-9 rows flipped to `impl` at v0.11.0**; see next
section.

## Release: v0.11.0 (Wave 9, 2026-04-26)

`PostgresBackend::capabilities()` reports `true` for the 12 Wave-9
flags (`cancel_execution`, `change_priority`, `replay_execution`,
`revoke_lease`, `read_execution_state`, `read_execution_info`,
`get_execution_result`, `budget_admin`, `quota_admin`,
`list_pending_waitpoints`, `cancel_flow_header`, `ack_cancel_member`).
Every Stage-A table row above is now `impl | impl` except
`subscribe_instance_tags` (`n/a` on both per #311) and the retired
`fetch_waitpoint_token_v07` (removed at v0.8.0).

### Parity summary at v0.11.0

| Family | Valkey | Postgres | Notes |
|---|---|---|---|
| Ingress (5 methods) | `impl` | `impl` | — |
| Flow family (6 methods) | `impl` | `impl` | — |
| Flow cancel (`cancel_flow_header`, `ack_cancel_member`) | `impl` | `impl` | **Wave 9.** Postgres cancel-backlog via migration 0014. |
| Read model (`read_execution_info`, `read_execution_state`, `get_execution_result`) | `impl` | `impl` | **Wave 9.** Postgres join-on-read against `ff_exec_core` + `ff_attempt`. |
| Operator control (`cancel_execution`, `change_priority`, `replay_execution`, `revoke_lease`) | `impl` | `impl` | **Wave 9.** Postgres SERIALIZABLE-fn template + `ff_operator_event` / `ff_lease_event` outbox emit. |
| Budget / quota (`create_budget`, `reset_budget`, `create_quota_policy`, `get_budget_status`, `report_usage_admin`) | `impl` | `impl` | **Wave 9.** Postgres migrations 0012 + 0013 + Postgres-native `BudgetResetReconciler`. |
| Scheduler (`claim_for_worker`) | `impl` | `impl` | — |
| Waitpoints (`list_pending_waitpoints`) | `impl` | `impl` | **Wave 9.** Postgres `ff_waitpoint_pending` with migration 0011 columns. |
| Admin rotation + seed | `impl` | `impl` | — |
| Cross-cutting | `impl` | `impl` | — |
| `subscribe_instance_tags` | `n/a` | `n/a` | Deferred per #311 (speculative demand). |

### Wave 9 — shipped v0.11.0 (2026-04-26)

Full design record: [`rfcs/RFC-020-postgres-wave-9.md`](../rfcs/RFC-020-postgres-wave-9.md)
(ACCEPTED 2026-04-26, Revision 7). All 13 Wave-9 method rows flipped
`stub → impl` on the Postgres column at v0.11.0; the
`subscribe_instance_tags` row remains `n/a` on both backends per #311
(speculative demand, served by `list_executions` +
`ScannerFilter::with_instance_tag` today).

Migrations shipped as part of Wave 9 (all additive, forward-only):

- **0010** `ff_operator_event` outbox — new LISTEN/NOTIFY channel for
  operator-control events (`priority_changed` / `replayed` /
  `flow_cancel_requested`), preserving the RFC-019 `ff_signal_event`
  subscriber contract.
- **0011** `ff_waitpoint_pending` additive columns (`state`,
  `required_signal_names`, `activated_at_ms`) — required to serve the
  real `PendingWaitpointInfo` contract.
- **0012** `ff_quota_policy` family (`ff_quota_policy` +
  `ff_quota_window` + `ff_quota_admitted`, 256-way HASH-partitioned on
  `partition_key`).
- **0013** `ff_budget_policy` additive columns — `next_reset_at_ms`
  + 4 breach-tracking + 3 definitional columns; enables a Postgres-
  native `BudgetResetReconciler`.
- **0014** `ff_cancel_backlog` table — per-member cancel tracking
  driving `cancel_flow_header` + `ack_cancel_member` + the
  cancel-backlog reconciler.

Consumer migration notes at [`CONSUMER_MIGRATION_0.11.md`](CONSUMER_MIGRATION_0.11.md).

## Release: v0.12.0 (SQLite dev-only backend, RFC-023, 2026-04-26)

Third `EngineBackend` implementation lands at v0.12:
`ff-backend-sqlite`. Scoped **permanently** to dev-only / testing
per [`rfcs/RFC-023-sqlite-dev-only-backend.md`](../rfcs/RFC-023-sqlite-dev-only-backend.md)
§1.0. See [`docs/dev-harness.md`](dev-harness.md) for the consumer
setup guide and
[`docs/CONSUMER_MIGRATION_0.12.md`](CONSUMER_MIGRATION_0.12.md) for
the upgrade checklist.

**Positioning reminder.** SQLite is a testing harness; Valkey is
the engine; Postgres is the enterprise persistence layer. The
parity rows below do NOT imply perf parity — SQLite is a
single-writer, single-process, ~10³ write-QPS envelope. Production
scale demands Valkey or Postgres.

### Parity summary at v0.12.0 (all three backends)

| Family | Valkey | Postgres | SQLite | Notes |
|---|---|---|---|---|
| Ingress (5 methods) | `impl` | `impl` | `impl` | `create_flow`, `create_execution`, `add_execution_to_flow`, `stage_dependency_edge`, `apply_dependency_to_child`. |
| Flow family (6 methods) | `impl` | `impl` | `impl` | `describe_flow`, `list_flows`, `list_edges`, `describe_edge`, `cancel_flow`, `set_edge_group_policy`. |
| Flow cancel (`cancel_flow_header`, `ack_cancel_member`) | `impl` | `impl` | `impl` | **v0.12 (RFC-023 Phase 3.3).** SQLite ports the PG cancel-backlog semantics (migration 0014 analogue); `AlreadyTerminal { stored_* }` idempotent replay matches PG. Operator-event outbox INSERT in-tx + post-commit broadcast wakeup. |
| Read model (`read_execution_info`, `read_execution_state`, `get_execution_result`) | `impl` | `impl` | `impl` | **v0.12 (RFC-023 Phase 3.3).** SQLite lowers PG's `LEFT JOIN LATERAL` to correlated subqueries; storage-tier literal normalisation reuses the PG helper shape. |
| Operator control (`cancel_execution`, `change_priority`, `replay_execution`, `revoke_lease`) | `impl` | `impl` | `impl` | **v0.12 (RFC-023 Phase 3.2).** SQLite runs the PG Rev-7 spine under `BEGIN IMMEDIATE` RESERVED-lock + WHERE-clause CAS fencing + `retry_serializable` for SQLITE_BUSY absorption. `ff_lease_event` + `ff_operator_event` outbox emits match PG exactly. |
| Budget / quota (`create_budget`, `reset_budget`, `create_quota_policy`, `get_budget_status`, `report_usage_admin`) | `impl` | `impl` | `impl` | **v0.12 (RFC-023 Phase 3.4).** SQLite hand-ports PG `ff_budget_policy` / `ff_quota_policy` family with positional `?` placeholders; breach-counter columns (`breach_count`, `soft_breach_count`, `last_breach_at_ms`, `last_breach_dim`) maintained in-tx matching Valkey + PG Rev-6. |
| Waitpoints (`list_pending_waitpoints`) | `impl` | `impl` | `impl` | **v0.12 (RFC-023 Phase 3.3).** SQLite cursor-paginated scan of `ff_waitpoint_pending` with `state IN ('pending','active')` filter + `NotFound` on missing execution + `(token_kid, token_fingerprint)` parsed from the stored token. |
| Scheduler (`claim_for_worker`) | `impl` | `impl` | `stub` | **SQLite non-goal** per RFC-023 §5 — no scheduler is wired on SQLite. `Supports::claim_for_worker = false` on the SQLite backend. Dev harness consumers drive claim/complete/fail directly through the `EngineBackend` trait. |
| Subscribe (lease / completion / signal) | `impl` | `impl` | `impl` | **v0.12.** SQLite uses `tokio::sync::broadcast` channels for WAKEUP + outbox tables (migrations 0006 / 0007 / 0010 analogues) for cursor-resume, mirroring the RFC-019 contract. In-process only — cross-process subscribe fan-out is a PG-only property. |
| `subscribe_instance_tags` | `n/a` | `n/a` | `n/a` | Deferred per #311 (speculative demand; served by `list_executions` + `ScannerFilter::with_instance_tag`). |
| Admin rotation + seed | `impl` | `impl` | `impl` | — |
| Tags (`set_execution_tag`, `set_flow_tag`, `get_execution_tag`, `get_flow_tag`) | `impl` | `impl` | `impl` | **v0.12 follow-up (issue #433).** Operator/control-plane point-writes + reads for caller-namespaced tags (e.g. `cairn.session_id`). Valkey routes to `ff_set_{execution,flow}_tags` Lua; PG/SQLite upsert `raw_fields` JSON(B) (`tags.<k>` for exec, top-level `<k>` for flow). Trait-side `ff_core::engine_backend::validate_tag_key` enforces the full-key regex `^[a-z][a-z0-9_]*\.[a-z0-9_][a-z0-9_.]*$` on every backend (stricter than Valkey's prefix-only Lua check — the Rust gate is the parity-of-record); read-side missing-row collapses to `Ok(None)`. |
| Cross-cutting (`backend_label`, `ping`, `shutdown_prepare`, `prepare`) | `impl` | `impl` | `impl` | SQLite `prepare` returns `PrepareOutcome::NoOp` (migrations run via `sqlx::migrate!` at pool init); `shutdown_prepare` drains the N=1 scanner supervisor. |

### SQLite-specific notes

- **No partitioning.** SQLite drops `PARTITION BY HASH` — one
  non-partitioned table per entity (RFC-023 §4.1). The scanner
  supervisor collapses to `N=1` (one tick task per reconciler, no
  fan-out).
- **Single-writer + retry classifier.** SERIALIZABLE-grade ops run
  under `BEGIN IMMEDIATE` + `retry_serializable` wrapping of
  `is_retryable_sqlite_busy` (`SQLITE_BUSY` / `SQLITE_BUSY_TIMEOUT` /
  `SQLITE_LOCKED`). `MAX_ATTEMPTS = 3` matches PG's
  `CANCEL_FLOW_MAX_ATTEMPTS`.
- **Production guard.** `SqliteBackend::new` refuses to construct
  without `FF_DEV_MODE=1` and emits a WARN banner on every
  construction.
- **Migrations.** SQLite migrations 0001 – 0014 are hand-ported
  SQLite-dialect files 1:1 numbered with Postgres for parity-drift
  detection (RFC-023 §4.1). CI lints the pairing via a
  `.sqlite-skip` sidecar allow-list.
- **Handle codec wire byte `0x03`.** `BackendTag::Sqlite`
  (wire byte `0x03`) joins `Valkey=0x01` / `Postgres=0x02`; handles
  minted by one backend are rejected by the other two with
  `EngineError::Validation { kind: HandleFromOtherBackend }`.

Phase 4c (capability-matrix snapshot test) flips the `Supports`
flags above from `Supports::none()` at release time; the `impl`
entries here are the post-flip state. See
`crates/ff-backend-sqlite/tests/capabilities.rs` for the snapshot
gate.

### RFC-024 + agnostic-SDK trait surface at v0.12.0

The ten `EngineBackend` trait methods below are the v0.12 trait
surface relevant to this release — nine are new in v0.12 (RFC-024
lease-reclaim + the agnostic-SDK PR-1..PR-5.5 trait-routed read
primitives, scanner primitives, and grant-consumer dispatch);
`deliver_signal` predates v0.12 on the trait and is included here
because the SDK caller is no longer `valkey-default`-gated in v0.12.
Row values reflect actual bodies on `main` at tag-prep time —
per-backend `grep 'fn <method>' crates/ff-backend-*/src/**` against
the trait defaults in `crates/ff-core/src/engine_backend.rs`.

| Method | Valkey | Postgres | SQLite | Notes |
|---|---|---|---|---|
| `issue_reclaim_grant` | `impl` | `impl` | `impl` | **RFC-024.** PR-F (Valkey), PR-D (PG), PR-E (SQLite). Admission write for the lease-reclaim path; rejects `NotReclaimable` on non-`active` lifecycle or capability-mismatch. |
| `reclaim_execution` | `impl` | `impl` | `impl` | **RFC-024.** Grant-consumer for reclaim; mints a fresh attempt on `lease_expired_reclaimable` / `lease_revoked`. All three backends enforce `max_reclaim_count` (default 1000) + emit `HandleKind::Reclaimed`. |
| `read_execution_context` | `impl` | `impl` | `impl` | **Agnostic-SDK PR-1** (#411). Point-read of `ExecutionContext` for the SDK worker's resume path. Missing row surfaces as `Validation { kind: InvalidInput }` on all three — SDK only invokes post-claim so a missing row is a loud invariant violation. |
| `read_current_attempt_index` | `impl` | `impl` | `impl` | **Agnostic-SDK PR-3.** Documented asymmetry (rustdoc on `EngineBackend::read_current_attempt_index`): Valkey returns `AttemptIndex(0)` when `exec_core` is present but `current_attempt_index` is absent/empty (pre-PR-3 inline parity); PG/SQLite use `NOT NULL DEFAULT 0` so a pre-claim row reads `0` naturally, but a missing row surfaces as `Validation { kind: InvalidInput }`. The downstream `claim_resumed_execution` FCALL / SQL surfaces the proper `NotAResumedExecution` / `ExecutionNotLeaseable` reject. |
| `read_total_attempt_count` | `impl` | `impl` | `impl` | **Agnostic-SDK PR-5.5** (#419). Valkey HGET on `{exec}:core` (monotonic total-claims counter, distinct from `current_attempt_index`); PG reads `raw_fields` JSONB extract via `exec_core::read_total_attempt_count_impl`; SQLite reads via `json_extract` in `crates/ff-backend-sqlite/src/reads.rs`. |
| `claim_execution` | `impl` | `Unavailable` (default) | `Unavailable` (default) | **Agnostic-SDK PR-4** (#417). Trait-routed grant-consumer for the SDK `claim_from_grant` path. Valkey fires one `ff_claim_execution` FCALL; PG/SQLite inherit the `Err(Unavailable { op: "claim_execution" })` default. PG's grant-consumer flow today routes through `PostgresScheduler::claim_for_worker` — a separate scheduler-side entry point distinct from this trait method; SQLite has no grant-consumer path. Full trait-level grant-consumer parity for PG + SQLite is v0.13 RFC-scope; see [`CONSUMER_MIGRATION_0.12.md`](CONSUMER_MIGRATION_0.12.md) §Known limitations. |
| `scan_eligible_executions` | `impl` | `Unavailable` (default) | `Unavailable` (default) | **Agnostic-SDK PR-5** (#418). Scanner-bypass primitive — lane-eligible ZSET peek. Valkey-only by design; PG/SQLite consumers use the scheduler-routed `claim_for_worker` path which handles eligibility server-side. Exposed behind the `direct-valkey-claim` bench-only feature; not a general consumer surface. |
| `issue_claim_grant` | `impl` | `Unavailable` (default) | `Unavailable` (default) | **Agnostic-SDK PR-5.** Scheduler-bypass claim-grant write. Pairs with `scan_eligible_executions`; same Valkey-only scope and `direct-valkey-claim` feature gating. PG/SQLite consumers use `claim_for_worker`. |
| `block_route` | `impl` | `Unavailable` (default) | `Unavailable` (default) | **Agnostic-SDK PR-5.** Moves an execution from lane-eligible ZSET to `blocked_route` ZSET after a capability-mismatch reject. Valkey-only; PG/SQLite admission rejects are handled server-side inside the scheduler-routed `claim_for_worker` path. |
| `deliver_signal` | `impl` | `impl` | `impl` | **Agnostic-SDK PR-3** (#416). Method was already trait-routed pre-v0.12 across all three backends; the v0.12 change is removing the `valkey-default`-feature module gate on the SDK caller so consumers compiling `--no-default-features --features sqlite` (or `postgres`) can reach it. |

Consumer-facing limitations for this surface live in
[`CONSUMER_MIGRATION_0.12.md`](CONSUMER_MIGRATION_0.12.md) §Known
limitations.

### cairn #389 — service-layer typed FCALL surface (v0.13)

Six `EngineBackend` methods that mirror existing Handle-taking peers
(`complete` / `fail` / `renew`) but accept `(execution_id, fence)`
tuples — lets control-plane callers (cairn's
`valkey_control_plane_impl.rs`, future non-Handle consumers) drop
the raw `ferriskey::Value` + `check_fcall_success` + `parse_*`
pattern. Args/Result types already existed in `ff_core::contracts`;
this landing adds the trait-method surface.

Valkey ships bodies at landing; PG + SQLite inherit
`EngineError::Unavailable` until follow-up parity work — same
staging as RFC-024 reclaim primitives + `claim_execution` above.

| Method | Valkey | Postgres | SQLite | Notes |
|---|---|---|---|---|
| `complete_execution` | `impl` | `Unavailable` (default) | `Unavailable` (default) | Service-layer peer of `complete(handle)`. Valkey pre-reads `lane_id` + `current_worker_instance_id` from `exec_core` then delegates to `ff_complete_execution`. |
| `fail_execution` | `impl` | `Unavailable` (default) | `Unavailable` (default) | Service-layer peer of `fail(handle, …)`. Same `exec_core` pre-read pattern; delegates to `ff_fail_execution`. |
| `renew_lease` | `impl` | `Unavailable` (default) | `Unavailable` (default) | Service-layer peer of `renew(handle)`. No pre-read needed — `ExecKeyContext` suffices. Delegates to `ff_renew_lease`. |
| `resume_execution` | `impl` | `Unavailable` (default) | `Unavailable` (default) | Lifecycle transition from suspended to runnable. Valkey pre-reads `lane_id` + `current_waitpoint_id` from `exec_core` then delegates to `ff_resume_execution`. |
| `check_admission` | `impl` | `Unavailable` (default) | `Unavailable` (default) | Atomic admission check against a quota policy. Takes `quota_policy_id: &QuotaPolicyId` and `dimension: &str` outside the `CheckAdmissionArgs` struct — quota keys live on the `{q:<policy>}` partition that cannot be derived from `execution_id`, and widening the existing pub-fields struct would semver-break. Empty `dimension` → `"default"`. Delegates to `ff_check_admission_and_record`. |
| `evaluate_flow_eligibility` | `impl` | `Unavailable` (default) | `Unavailable` (default) | Read-only eligibility status (`eligible`, `blocked_by_dependencies`, …). Delegates to `ff_evaluate_flow_eligibility`. |

PG + SQLite parity for this surface is the v0.13 follow-up scope.
Until the bodies land, consumers compiling against those backends
receive `EngineError::Unavailable { op: "<name>" }` on dispatch — a
terminal, non-retryable classification the `ErrorClass` mapping
already handles.
