# RFC-017: ff-server backend abstraction — migrating HTTP handlers onto `Arc<dyn EngineBackend>`

**Status:** Draft (consolidated master)
**Authors:** Worker-A + Worker-B (independent divergent drafts); consolidated by manager, 2026-04-23
**Created:** 2026-04-23
**Divergent inputs:** `rfcs/drafts/RFC-017-ff-server-backend-abstraction-A.md` (PR #259, 486 lines), `rfcs/drafts/RFC-017-ff-server-backend-abstraction-B.md` (PR #260, 400 lines). Both drafts are preserved in-tree as the divergent-author record; this document is authoritative.
**Related RFCs:** RFC-006 (stream tail mux), RFC-011 (co-location + `{fp:N}`), RFC-012 (EngineBackend trait + Round-7 stabilisation), RFC-016 (quorum edges staged rollout).
**Parent context:** `rfcs/drafts/v0.7-migration-master.md` (Wave 8 readiness), `rfcs/drafts/v0.7-migration-survey-engine.md` §§4-6.

---

## Summary

`ff-server` is the last Valkey-hardcoded surface in the v0.7 stack. Every `Server::<method>` in `crates/ff-server/src/server.rs` still reaches directly into `ferriskey::Client` via `self.client.fcall_with_reload(...)`, `self.client.cmd(...)`, or `self.tail_client.cmd(...)`, constructing KEYS/ARGV arrays positionally and parsing raw `ferriskey::Value` replies. Wave 4-7 landed a Postgres `EngineBackend`, but no HTTP consumer can reach it because `Server::start(ServerConfig)` hardcodes a ferriskey dial and 23 handler call-sites bypass the trait.

This RFC migrates `Server` onto a single `Arc<dyn EngineBackend>` field, expands the trait to cover every op `ff-server` serves in steady state (adding **20** new methods — ingress 5, operator control 4, budget/quota admin 5 (incl. `report_usage_admin`), read+diagnostics 3, scheduling 1, cross-cutting 2 — bringing the trait from **31 → 51 methods**; see §2.3 for the authoritative breakdown), relocates all Valkey-specific primitives (`tail_client`, `stream_semaphore`, `xread_block_lock`) from `Server` into `ValkeyBackend`, and stages the migration in four CI-green stages plus a v0.8 deprecation cleanup. **`FF_BACKEND=postgres` is hard-gated to Stage E (v0.8.0) per §9** — no silent 501s mid-migration.

The ingress + admin promotion is a **breaking trait change** — every external `impl EngineBackend for X` recompiles. The consolidated position: ship it as **v0.8.0**, not as v0.7.x. Rationale in §10.

**Consolidated positions (both drafts agreed):**

1. Single-trait expansion; no `IngressBackend` / `AdminBackend` split.
2. `Server` holds **one** `Arc<dyn EngineBackend>` field; no enum, no type parameter, no `Any` downcasting.
3. Valkey primitives (`tail_client`, `stream_semaphore`, `xread_block_lock`) live inside `ValkeyBackend`, not on `Server`.
4. `Server::start(ServerConfig)` keeps its signature; a new `start_with_backend(...)` lands alongside for test injection.

**Consolidated positions (resolving A/B divergences; see §3 for the resolution log):**

5. **From A (round-1 F12 revised):** add `shutdown_prepare(grace: Duration) -> Result<(), EngineError>` to the trait; **no default impl** — Valkey closes the stream semaphore + drains up to `grace`, Postgres issues `UNLISTEN *` + drains sqlx pool up to `grace`. Per-backend spec in §5.4.
6. **From B:** add `backend_label() -> &'static str` to the trait for observability dimensioning.
7. **From B (round-1 F5 revised):** `list_pending_waitpoints` sanitises the HMAC token across the trait boundary — 6 existing fields preserved (`waitpoint_key`, `state`, `required_signal_names`, `activated_at`, `created_at`, `expires_at`), `waitpoint_token` dropped, `(execution_id, token_kid, token_fingerprint)` added. Additive redaction, not wholesale rewrite. Details in §8.
8. **Manager call:** scheduler cursor state stays scoped to the owning backend crate (`ff_scheduler::Scheduler` under Valkey; `PostgresScheduler` under Postgres) and `Server` borrows it through a trait-method accessor on `EngineBackend::claim_for_worker`, not through a separate `SchedulerBackend` trait. Both schedulers are already backend-scoped in v0.7 (Wave 5b PR #246); this RFC consolidates the server-side dispatch.

---

## 1. Motivation

### 1.1 Wave 8 gap

The v0.7 migration assumed "swap the client, wire the trait" for `ff-server`. The engine survey (`v0.7-migration-survey-engine.md` §5.2) flagged the fact plainly: *"all Valkey call-sites concentrate in `server.rs`; the HTTP-to-backend seam is `Server::<method>()`."* That seam is the right abstraction boundary — but it currently terminates at ferriskey, not at the trait.

Consequence: a Postgres-backed cairn-fabric deployment **cannot use `ff-server`**. The only two paths today are:

1. Use `ff-sdk` directly and bypass the REST surface entirely (what `deploy-approval` does); or
2. Fork `ff-server` and replace every `self.client.*` call site.

Neither is acceptable once v0.7 is framed as "Postgres parity."

### 1.2 Why this is architecture, not cleanup

Five `Server::` methods — `create_execution`, `create_flow`, `add_execution_to_flow`, `stage_dependency_edge`, `apply_dependency_to_child` — are **not on `EngineBackend` at all**. They are also **not** inherent methods on `ValkeyBackend`: the Valkey path authored them directly on `Server`, inline, against `ferriskey::Client`. On the Postgres side the same five exist as **inherent methods on `PostgresBackend`** (`crates/ff-backend-postgres/src/lib.rs:184-238`), placed there because Wave 4i needed the ingress path before the trait could be extended.

This is **backend drift**. The two concrete backends have different public shapes — not at the trait level, but one layer below — and the HTTP handler layer only works against Valkey because it bypasses the abstraction. RFC-012 §4.2 framed this seam but the ingress surface was out of scope. This RFC closes it.

### 1.3 Three concrete pains

- **No Postgres over HTTP.** v0.7's Postgres backend lands as a library (`ff-backend-postgres`) but consumers reach FlowFabric via `ff-server`'s REST API.
- **No mockable handler tests.** `ff-server` unit tests spin up a real Valkey container. The backend trait is the natural seam for `MockBackend`; the current `Server` blocks it (the raw `ferriskey::Client` field isn't mockable).
- **Leaky abstractions.** `Server::rotate_waitpoint_secret` already knows about `Partition`, `num_flow_partitions`, and the FCALL name `ff_rotate_waitpoint_hmac_secret`. The HTTP layer has no business knowing any of those.

### 1.4 Scope

In scope:

- `Server` struct + all 24 HTTP handler methods on it.
- Trait expansion: +20 new methods across ingress (5), operator control (4), budget/quota admin (5), read/diagnostics (3), scheduling (1), cross-cutting (2). Authoritative breakdown in §2.3.
- Admin: `/v1/admin/rotate-waitpoint-secret` migrates to `EngineBackend::rotate_waitpoint_hmac_secret_all` (already on the trait).
- `ServerConfig` gains `backend: BackendConfig` (sum-typed: Valkey or Postgres).

Out of scope:

- Changing REST wire format (route paths, request/response JSON).
- Engine + scheduler changes — already behind `Arc<dyn EngineBackend>` post-Wave 4/5.
- Worker / SDK changes — consumers of the trait already work.
- Adding new HTTP endpoints. Migrate-then-extend.
- Replacing the `CompletionBackend` trait — separate surface.
- Dual-backend per-process.

---

## 2. Current state — authoritative handler inventory

### 2.1 `Server` struct today

```rust
pub struct Server {
    client: Client,                               // main ferriskey client
    tail_client: Client,                          // dedicated mux for XREAD/XRANGE (RFC-006)
    stream_semaphore: Arc<Semaphore>,             // 429-on-contention gate
    xread_block_lock: Arc<Mutex<()>>,             // serialises XREAD BLOCK on the tail mux
    admin_rotate_semaphore: Arc<Semaphore>,       // server-wide rotate fairness gate
    engine: Engine,
    config: ServerConfig,
    scheduler: Arc<ff_scheduler::Scheduler>,
    background_tasks: Arc<AsyncMutex<JoinSet<()>>>,
    metrics: Arc<ff_observability::Metrics>,
}
```

Four fields (`client`, `tail_client`, `stream_semaphore`, `xread_block_lock`) are Valkey-artifact-only. The remaining eight are backend-agnostic **in shape**.

### 2.2 Authoritative handler inventory

Reconciled against `crates/ff-server/src/server.rs` (4730 lines, as of main @7076bc1). Grep of `self\.(client|tail_client|fcall_with_reload)` yields 23 direct call-sites (A's table was correct; B's "14 methods with +12 additions" was a handler-family count, not a call-site count — they are reconciled below).

All 24 HTTP-facing handler methods on `Server`:

| # | Server method | Line | Backend primitive today | Trait method | Effort | Category |
|---|---|---|---|---|---|---|
| 1 | `create_execution` | 670 | `FCALL ff_create_execution` | **NEW** | M | Ingress |
| 2 | `cancel_execution` | 754 | `FCALL ff_cancel_execution` + HMGET pre-read | **NEW** | M | Operator control |
| 3 | `get_execution_state` | 786 | `HGET exec_core public_state` | reuse `describe_execution` | T | Read |
| 4 | `get_execution_result` | 840 | raw `GET <result key>` | **NEW** `get_execution_result` | M | Read |
| 5 | `list_pending_waitpoints` | 911 | SSCAN + 2× pipelined HMGET | **NEW** (see §8) | H | Read + security |
| 6 | `create_budget` | 1176 | `FCALL ff_create_budget` | **NEW** | M | Budget admin |
| 7 | `create_quota_policy` | 1236 | `FCALL ff_create_quota_policy` | **NEW** | M | Quota admin |
| 8 | `get_budget_status` | 1274 | 3× HGETALL | **NEW** | M | Budget admin |
| 9 | `report_usage` | 1350 | `FCALL ff_report_usage_and_check` | existing `report_usage` (see §5) | H | Budget (admin-path shape) |
| 10 | `reset_budget` | 1393 | `FCALL ff_reset_budget` | **NEW** | M | Budget admin |
| 11 | `create_flow` | 1421 | `FCALL ff_create_flow` | **NEW** | M | Ingress |
| 12 | `add_execution_to_flow` | 1487 | `FCALL ff_add_execution_to_flow` | **NEW** | M | Ingress |
| 13 | `cancel_flow` / `cancel_flow_wait` | 1586 / 1596 | `FCALL ff_cancel_flow` + server-local async dispatch | existing `cancel_flow` (header only) | H | Control |
| 14 | `stage_dependency_edge` | 1913 | `FCALL ff_stage_dependency_edge` | **NEW** | M | Ingress |
| 15 | `apply_dependency_to_child` | 1957 | HGET lane + `FCALL ff_apply_dependency_to_child` | **NEW** | M | Ingress |
| 16 | `deliver_signal` | 2022 | HGET lane + `FCALL ff_deliver_signal` | existing `deliver_signal` | T | Control |
| 17 | `change_priority` | 2123 | HGET lane + `FCALL ff_change_priority` | **NEW** | M | Operator control |
| 18 | `claim_for_worker` | 2172 | `ff_scheduler::Scheduler::claim_for_worker` | **NEW** `claim_for_worker` | M | Scheduling (see §7) |
| 19 | `revoke_lease` | 2199 | HGET wiid + `FCALL ff_revoke_lease` | **NEW** | M | Operator control |
| 20 | `get_execution` | 2249 | HGETALL exec_core | existing `describe_execution` (shape drift) | M | Read |
| 21 | `list_executions_page` | 2351 | SMEMBERS + sort + slice | existing `list_executions` | T | Read |
| 22 | `replay_execution` | 2414 | HMGET + SMEMBERS + FCALL (variadic KEYS) | **NEW** | H | Operator control |
| 23 | `read_attempt_stream` | 2525 | XRANGE via `ff_script::stream` + stream_semaphore | existing `read_stream` | T | Stream |
| 24 | `tail_attempt_stream` | 2605 | XREAD BLOCK + stream_semaphore + xread_block_lock | existing `tail_stream` | T | Stream |
| + | `rotate_waitpoint_secret` | 3240 | per-partition FCALL fan-out | existing `rotate_waitpoint_hmac_secret_all` | T | Admin |
| + | `healthz` (in `api.rs`) | — | `PING` via client | **NEW** `ping` | T | Diagnostics |

Boot-time Valkey-only routines (move into `ValkeyBackend::connect`):

- `verify_valkey_version` (line 2776) — `INFO server`.
- `validate_or_create_partition_config` (2997).
- `initialize_waitpoint_hmac_secret` (3153) — per-partition HSET.
- `ensure_library` (Lua FUNCTION LOAD, 423).
- lanes SADD seed (443).

### 2.3 Reconciliation of A/B counts

- Author A counted **13 trait-method additions** (ingress 5 + execution-control 8: `cancel_execution`, `get_execution_result`, `create_budget`, `create_quota_policy`, `get_budget_status`, `reset_budget`, `change_priority`, `revoke_lease`, `replay_execution`, `claim_for_worker`) — note A double-counted on one row; true count is 13 new.
- Author B counted **12** (ingress 5 + operator control 3: `cancel_execution`, `change_priority`, `replay_execution`; budget/quota admin 4: `create_budget`, `reset_budget`, `create_quota_policy`, `get_budget_status`; diagnostics 1: `ping`) and folded `revoke_lease` + `claim_for_worker` + `list_pending_waitpoints` + `get_execution_result` as separate lines, and counted `ping` which A omitted.

**Reconciled count (authoritative, post-round-1 revision per K F1 + F4).** Direct grep of `origin/main:crates/ff-core/src/engine_backend.rs` inside the `trait EngineBackend` block yields **31 existing methods** (earlier drafts said 30 — off by one against `main`).

| Bucket | Methods | Count |
|---|---|---|
| Ingress | `create_execution`, `create_flow`, `add_execution_to_flow`, `stage_dependency_edge`, `apply_dependency_to_child` | 5 |
| Operator control | `cancel_execution`, `change_priority`, `replay_execution`, `revoke_lease` | 4 |
| Budget + quota admin | `create_budget`, `reset_budget`, `create_quota_policy`, `get_budget_status`, `report_usage_admin` | 5 |
| Read + diagnostics | `get_execution_result`, `list_pending_waitpoints`, `ping` | 3 |
| Scheduling | `claim_for_worker` | 1 |
| Cross-cutting | `shutdown_prepare`, `backend_label` | 2 |
| **Total new** | | **20** |

**Base 31 + new 20 = 51 methods post-RFC.** Per K's F1 + F4: every new method is counted exactly once; `report_usage_admin` is a first-class new method (not a folded extension of existing `report_usage`). Earlier "44 / +14" and "45 / +15" numbers are retired; **51 / +20** is the single authoritative figure, used everywhere in this revision.

The gap between A's 13 and B's 12 is that B folded `claim_for_worker` under "Scheduler holds its own `Arc<dyn EngineBackend>`, Server forwards" (not a trait add at this stage) while A promoted it to the trait. The consolidation keeps `claim_for_worker` on the trait (§7) — both backends already own a scheduler module; trait-lifting is mechanical and eliminates a branch on `Server`.

### 2.3.1 Call-site density and risk classification

Grep of `self\.(client|tail_client|fcall_with_reload)` in `crates/ff-server/src/server.rs` returns 23 direct call-sites concentrated across lines 637-3402. Density map:

- Lines 637-779: `fcall_with_reload` helper + `create_execution` + `cancel_execution` — 4 call-sites. Ingress + control hot path.
- Lines 786-1107: read ops (`get_execution_state`, `get_execution_result`, `list_pending_waitpoints`) — 3 call-sites, 2 of them inside pipelines (HGET + SSCAN pre-reads).
- Lines 1176-1421: budget + quota admin — 4 call-sites (3 FCALLs + 1 HGETALL family).
- Lines 1421-1957: flow ingress + dependency edges — 5 call-sites. Ingress core.
- Lines 2022-2414: signal dispatch + operator control + replay — 6 call-sites.
- Lines 2525-2664: stream ops on `tail_client` — 3 call-sites using the dedicated mux.
- Line 3240 + 3388-3402: admin rotate fan-out — 1 outer + 1 per-partition call-site.

**Boot-time, off the hot path** (not counted in the 23 runtime sites):

- Line 423: `FUNCTION LOAD` (Lua library).
- Line 443: `SADD ff:idx:lanes` seed.
- Line 2776: `INFO server` version check.
- Line 2997: `HGETALL partition_config` validation.
- Line 3153: per-partition `HSET` HMAC initialisation.

Boot call-sites are **all** Valkey-exclusive; none have Postgres analogues. They move wholesale into `ValkeyBackend::connect_with_metrics` in Stage D, leaving `Server::start_with_metrics` ignorant of Valkey boot steps.

**Risk classification per region:**

| Region | Call-sites | Risk | Stage |
|---|---|---|---|
| Ingress (create_flow, add_execution_to_flow, stage/apply dep, create_execution) | 6 | High — Lua KEYS/ARGV positional contract | D |
| Operator control (cancel_execution, change_priority, replay, revoke_lease) | 5 | Medium — variadic KEYS in replay is the outlier | C |
| Budget + quota admin | 4 | Medium — dim-count validation currently server-side; stays server-side | C |
| Reads (get_state, get_result, list_pending_waitpoints) | 3 | Low (state) to High (pending-waitpoints pipeline) | B |
| Stream ops (tail_client) | 3 | Low — trait already has `read_stream`/`tail_stream`; migration is primitive relocation | B |
| Admin rotate fan-out | 2 | Low — trait already has `rotate_waitpoint_hmac_secret_all` | B |

Total: 23 runtime call-sites, covering 24 `Server::` methods. `claim_for_worker` + `healthz` are not in the grep because they route through the scheduler module and `api.rs` respectively.

### 2.4 Ingress op asymmetry

| Op | ff-sdk | `Server` (HTTP) | `ValkeyBackend` | `PostgresBackend` |
|---|---|---|---|---|
| `create_execution` | direct wrapper | inline FCALL | ✗ | **inherent** |
| `create_flow` | direct wrapper | inline FCALL | ✗ | **inherent** |
| `add_execution_to_flow` | direct wrapper | inline FCALL | ✗ | **inherent** |
| `stage_dependency_edge` | direct wrapper | inline FCALL | ✗ | **inherent** |
| `apply_dependency_to_child` | direct wrapper | inline FCALL | ✗ | **inherent** |

**Neither concrete backend exposes ingress on the trait.** This RFC closes the asymmetry.

---

## 3. Design — the shape

### 3.1 Trait expansion (consolidated)

**Position: single-trait expansion.** The trait grows from **31 to 51 methods** (+20; see §2.3 for the authoritative breakdown).

Rationale (consolidated from A §3.1 and B §3.1):

1. **Symmetry with Postgres.** `PostgresBackend` already has the five ingress ops as public inherent methods with identical shapes (`&CreateFlowArgs` / `&AddExecutionToFlowArgs` / etc). Trait-lifting is mechanical: declare the signatures on `EngineBackend`, add Valkey impls that wrap existing FCALL helpers in `ff-script::functions::*`, remove `#[cfg(feature = "core")]` from the Postgres inherent methods and convert them to trait impls.

2. **No legitimate deployment mode omits ingress.** A backend that cannot create executions cannot serve `/v1/executions POST`. Carving a second `FlowIngressBackend` implies a future where something implements `EngineBackend` but not ingress — a null set.

3. **Feature-flag propagation hazard.** Gating ingress under `#[cfg(feature = "core")]` (where it lives on `PostgresBackend` today) means consumers who disable `core` build backends that silently reject the five most common HTTP routes. Moving ingress onto the trait propagates the requirement explicitly (see feedback_feature_flag_propagation memory).

4. **Consumer symmetry.** `ff-server` wants ingress + worker + admin ops. `ff-sdk` wants worker ops. `migration-tool` wants admin ops. In every case the backing concrete type implements all three families. A split means three `Arc<dyn>` fields on `Server`, each pointing at the same `Arc<ValkeyBackend>` — pure ceremony (B §3.1).

5. **Object-safety hygiene.** The `_assert_dyn_compatible` guard at `engine_backend.rs:664` is one compile-time check. Three traits = three checks, three chances to regress.

6. **RFC-012 R7 precedent.** Round-7 expanded the trait to 31 methods and widened `append_frame` / `report_usage`. +20 is a larger step but the same pattern — additive, default-impl-protected, semver-gated.

### 3.2 `Server` struct (consolidated)

```rust
pub struct Server {
    backend: Arc<dyn EngineBackend>,            // replaces: client, tail_client
    admin_rotate_semaphore: Arc<Semaphore>,     // stays — server-wide fairness, backend-agnostic
    engine: Engine,                             // already backend-agnostic post-Wave 4
    config: ServerConfig,
    scheduler: Arc<ff_scheduler::Scheduler>,    // §7 — stays scoped to owning backend crate
    background_tasks: Arc<AsyncMutex<JoinSet<()>>>,
    metrics: Arc<ff_observability::Metrics>,
}
```

**Deleted:** `client`, `tail_client`, `stream_semaphore`, `xread_block_lock`. All four migrate into `ValkeyBackend`.

### 3.2.1 Why this shape, not a hybrid `Option<Arc<ValkeyStreamOps>>` capability handle

Author A's initial `§3.2` sketch included `stream_ops: Option<Arc<ValkeyStreamOps>>` as a capability handle on `Server` — the idea: keep a structured Valkey-only field for shutdown purposes, let handlers fall through to `backend.tail_stream` when present. A then revised to "no backend-specific field leaks onto `Server`" — we adopt the revised position.

Reasoning: any optional capability handle on `Server` implies a conditional code path in the handler ("if stream_ops is Some, use it; else fall through"). That conditional re-introduces the exact match-on-concrete-backend shape that §13.5 rejects. Cleaner: the trait owns the contract (`tail_stream` exists on every backend); the Valkey impl internally uses its own primitives; `Server` has no visibility into the Valkey-specific machinery.

Shutdown needs a seam regardless, which is why `shutdown_prepare()` exists on the trait (§3.3 D2). The Valkey impl of `shutdown_prepare` is the one place where the semaphore-close + tail_client-drop logic lives; `Server::shutdown` calls it before draining `background_tasks`. Postgres's impl defaults to no-op; a future backend with its own shutdown ritual gets one place to put it.

### 3.3 Divergence resolution log

| # | Topic | Author A | Author B | Consolidated |
|---|---|---|---|---|
| D1 | Total trait size | +13 (ingress 5 + exec-ctl 8) | +12 (ingress 5 + admin 7) | **+20** authoritative per §2.3; A + B both undercounted by folding supporting methods into handler rows |
| D2 | `shutdown_prepare()` | **proposed** | not addressed | **adopt A** — needed for Valkey semaphore drain + Postgres LISTEN detach |
| D3 | `backend_label()` | not addressed | **proposed** | **adopt B** — one `&'static str` method, metrics-essential |
| D4 | Scheduler location | field on `ValkeyBackend`, trait-lifted `claim_for_worker` | scheduler holds its own `Arc<dyn EngineBackend>`, Server forwards | **hybrid**: schedulers stay scoped to owning backend crate (Wave 5b pattern); `claim_for_worker` on the trait so `Server` calls one method (§7) |
| D5 | `list_pending_waitpoints` | complex HMGET pipeline, trait returns `Vec<PendingWaitpointInfo>` | same, **plus HMAC token not serialised** across trait boundary | **adopt B's security posture** — backend emits sanitised `PendingWaitpointInfo`, raw HMAC token never crosses the trait (§8) |
| D6 | Admin rotate fan-out | inside backend | inside backend | agreement |
| D7 | Boot sequence | move into `ValkeyBackend::connect` | companion `BackendBoot` trait | **boot inside backend's `connect_with_metrics`**, no `BackendBoot` trait; `Server::start` dispatches on `BackendConfig` enum. Eliminates an extra trait with one impl-per-backend (B §5.1 admitted it's convention-only) |
| D8 | Config shim | one-release deprecation, remove Stage D | deprecated in v0.7.0, removed v0.8.0 | **B's cadence** — aligns with semver boundary |
| D9 | Semver impact | "minor-version bump" loosely | **v0.8.0** explicitly | **v0.8.0** (§10) |
| D10 | Back-compat entry point | `Server::start(config)` preserved | `Server::start(config)` preserved + add `start_with_backend(...)` | **adopt B** — the second entry point unblocks `MockBackend` tests |
| D11 | CI gate alignment | stages merge at CI-green | stages **explicit CI-gate alignment** per feedback memo | **adopt B's wording** — each stage boundary is a CI-green workspace boundary |
| D12 | Postgres stream surface | `tail_stream` defaults to `Unavailable` on PG until follow-up | not addressed | **keep A** — Postgres streams wire through ff-server in a later RFC; v0.7 `tail_stream` returns `EngineError::Unavailable { op }` with the default impl; server maps to HTTP 501 |

---

## 4. Per-handler migration table

13 logical handler families (collapsing the 24 Server methods by HTTP-route family). T = Trivial (<1h), M = Moderate (~4h), H = Hard (>1d). Trait method = **NEW** if this RFC adds it; **existing** if already on `EngineBackend`.

| # | Handler family | Routes | Trait method | Effort | Notes |
|---|---|---|---|---|---|
| 1 | `healthz` | `GET /healthz` | **NEW** `ping() -> ()` | T | Valkey: `PING`; Postgres: `SELECT 1`. Eliminates last direct ferriskey call in `api.rs`. |
| 2 | Execution reads | `GET /v1/executions/{id}`, `.../state`, `.../result`, `GET /v1/executions` | existing `describe_execution` + existing `list_executions` + **NEW** `get_execution_result` | M | Shape drift (`ExecutionInfo` vs `ExecutionSnapshot`) adapted server-side (§8.2 of A, retained). `get_execution_result` is a new method because the payload blob is not part of `ExecutionSnapshot`; see Q3 in §12. |
| 3 | Operator control | `POST .../cancel`, `.../priority`, `.../replay`, `.../revoke-lease` | **NEW** `cancel_execution`, `change_priority`, `replay_execution`, `revoke_lease` | M–H | `replay_execution` is Hard because `ff_replay_execution` takes variadic KEYS based on inbound-edge count; trait signature hides it, Valkey impl does the pre-read internally. |
| 4 | Signal dispatch | `POST .../signal` | existing `deliver_signal` | T | One-line delegate. |
| 5 | Flow ingress | `POST /v1/flows`, `.../executions`, `.../cancel`, `.../edge`, `.../edge/{id}/apply` | **NEW** `create_flow`, `add_execution_to_flow`, `stage_dependency_edge`, `apply_dependency_to_child` + existing `cancel_flow` | M–H | `cancel_flow` trait covers the header only (Q2 adjudicated 2026-04-23 — see §16); member transitions via dispatcher cascade + reconciler backstop; callers needing sync semantics poll `describe_flow` until `public_flow_state = cancelled`. |
| 6 | Execution ingress | `POST /v1/executions` | **NEW** `create_execution` | M | `dedup_ttl_ms = 86400000` literal at `server.rs:734` lifts to `CreateExecutionArgs::idempotency_ttl: Option<Duration>` (default 24h). Zero wire impact. |
| 7 | Budget + quota admin | `POST /v1/budgets`, `.../budgets/{id}/status`, `.../budgets/{id}/usage`, `.../budgets/{id}/reset`, `.../quota-policies` | **NEW** `create_budget`, `get_budget_status`, `reset_budget`, `create_quota_policy` + existing-but-extended `report_usage` | M–H | See §5 on `report_usage` admin variant. |
| 8 | Pending-waitpoint listing | `GET .../pending-waitpoints` | **NEW** `list_pending_waitpoints` | **H** | Round-1 F3: upgraded M→H. Pipelined SSCAN + 2× HMGET + §8 schema rewrite + pagination-cursor new to the trait. Returns sanitised `PendingWaitpointInfo` — no raw HMAC token. §8. |
| 9 | Claim | `POST /v1/workers/{id}/claim` | **NEW** `claim_for_worker` | M | §7 — `ValkeyBackend` forwards to its `ff_scheduler::Scheduler` field; `PostgresBackend` forwards to `PostgresScheduler`. |
| 10 | Streams | `GET .../attempts/{n}/stream`, `.../stream/tail` | existing `read_stream` + existing `tail_stream` | T | `tail_client` + semaphore + mutex move into `ValkeyBackend` (§6). 429 semantics preserved via `EngineError::ResourceExhausted { pool, max }` → HTTP 429. |
| 11 | HMAC rotate | `POST /v1/admin/rotate-waitpoint-secret` | existing `rotate_waitpoint_hmac_secret_all` | T | Fan-out moves inside Valkey impl; admin semaphore + audit emit stay on `Server`. |
| 12 | Boot verification | (not HTTP-exposed) | N/A — lives inside `ValkeyBackend::connect_with_metrics` | **H** | Round-1 F3: added explicit score. `verify_valkey_version`, `ensure_library` (FUNCTION LOAD), `validate_or_create_partition_config`, `initialize_waitpoint_hmac_secret`, lanes SADD seed. **Ordering contract (load-bearing):** FUNCTION LOAD precedes lanes SADD seed (SADD scripts reference the library); HMAC init precedes lanes seed (pending-waitpoint reads depend on it). Ordering contract documented on `EngineBackend::connect_with_metrics` trait doc-comment, not just in each impl. |
| 13 | Shutdown | (not HTTP-exposed) | **NEW** `shutdown_prepare(grace: Duration)` | **H** | Round-1 F12: upgraded T→H. Signature takes a `Duration` grace budget; server wraps with top-level timeout. Valkey: closes `stream_semaphore`, awaits in-flight permits up to `grace`, drops `tail_client`. Postgres: issues `UNLISTEN *` + drains sqlx pool up to `grace`. **No default no-op** — each backend spec'd in §5. Guards against semaphore-close vs in-flight-acquire race (§14.8 mandatory Stage B CI test). |

Totals (post-round-1 F3/F12 rescoring): **5 trivial, 4 moderate, 4 hard** — `replay_execution`, `cancel_flow` dispatch, `report_usage` admin-path shape, `list_pending_waitpoints`, boot relocation, and `shutdown_prepare` are the Hard items (6 if boot + shutdown counted separately from handler families). Boot relocation + shutdown are Stage D + Stage B respectively; the others gate Stage C.

Round-1 F3 also clarified `create_execution` idempotency_ttl semantics: the `dedup_ttl_ms = 86400000` literal lifts as `CreateExecutionArgs::idempotency_ttl: Option<Duration>` with a backend-side default of 24h when `None`. **No HTTP-wire change** — the legacy route accepts no idempotency field today, and this revision keeps it that way; the `None` default preserves current behaviour byte-for-byte. A future wire extension is out of scope for RFC-017.

---

## 5. New trait methods — full list

New methods on `EngineBackend` (grouped; all `async fn` unless noted; default-impl strategy documented per method).

```rust
// ── Ingress (5) ──
async fn create_execution(&self, args: CreateExecutionArgs) -> Result<CreateExecutionResult, EngineError>;
async fn create_flow(&self, args: CreateFlowArgs) -> Result<CreateFlowResult, EngineError>;
async fn add_execution_to_flow(&self, args: AddExecutionToFlowArgs) -> Result<AddExecutionToFlowResult, EngineError>;
async fn stage_dependency_edge(&self, args: StageDependencyEdgeArgs) -> Result<StageDependencyEdgeResult, EngineError>;
async fn apply_dependency_to_child(&self, args: ApplyDependencyToChildArgs) -> Result<ApplyDependencyToChildResult, EngineError>;

// ── Operator control (4) ──
async fn cancel_execution(&self, args: CancelExecutionArgs) -> Result<CancelExecutionResult, EngineError>;
async fn change_priority(&self, args: ChangePriorityArgs) -> Result<ChangePriorityResult, EngineError>;
async fn replay_execution(&self, args: ReplayArgs) -> Result<ReplayResult, EngineError>;
async fn revoke_lease(&self, args: RevokeLeaseArgs) -> Result<RevokeLeaseResult, EngineError>;

// ── Budget + quota admin (5) — includes report_usage_admin per round-1 F4 ──
async fn create_budget(&self, args: CreateBudgetArgs) -> Result<CreateBudgetResult, EngineError>;
async fn reset_budget(&self, args: ResetBudgetArgs) -> Result<ResetBudgetResult, EngineError>;
async fn create_quota_policy(&self, args: CreateQuotaPolicyArgs) -> Result<(), EngineError>;
async fn get_budget_status(&self, id: &BudgetId) -> Result<BudgetStatus, EngineError>;
async fn report_usage_admin(&self, budget_id: &BudgetId, args: ReportUsageAdminArgs) -> Result<ReportUsageResult, EngineError>;

// ── Read + diagnostics (3) ──
async fn get_execution_result(&self, id: &ExecutionId) -> Result<Option<Vec<u8>>, EngineError>;
async fn list_pending_waitpoints(&self, args: ListPendingWaitpointsArgs) -> Result<ListPendingWaitpointsResult, EngineError>; // sanitised; see §8; cursor + limit
async fn ping(&self) -> Result<(), EngineError>;

// ── Scheduling (1) ──
async fn claim_for_worker(&self, args: ClaimForWorkerArgs) -> Result<ClaimForWorkerOutcome, EngineError>;

// ── Cross-cutting (2) ──
async fn shutdown_prepare(&self, grace: Duration) -> Result<(), EngineError>;  // NO default — each backend spec'd; see §5.4
fn backend_label(&self) -> &'static str;                                         // NO default — each backend overrides
```

**`report_usage_admin` placement (round-1 F4).** Promoted to a first-class entry in the budget+quota admin group above — not a folded extension of existing `report_usage`. The existing `report_usage(&Handle, ...)` continues to serve the worker-bound lease path; `report_usage_admin(&BudgetId, ReportUsageAdminArgs)` serves `/v1/budgets/{id}/usage` without worker ownership. Feature-flag placement: **`admin`** (see §5.2). Single-binary deployments (worker + operator in one process) compile with both `core` + `admin`; `core`-only deployments lose the admin path cleanly with a compile-level feature gate, not a runtime 501.

**Total trait size after RFC-017:** base 31 + 20 new = **51 methods** (per §2.3).

### 5.1 Method signature conventions

All new methods follow the conventions established by RFC-012 R7:

- **Args by owned struct**, not positional parameters. `CreateExecutionArgs`, `CancelExecutionArgs`, etc. Allows additive field growth without trait-signature churn.
- **Results as structured types**, not tuples. `CreateExecutionResult { execution_id, partition, created_at }` rather than `(ExecutionId, Partition, TimestampMs)`. Same reason.
- **Error: `EngineError`**, with backend-specific detail in `EngineError::Backend(BackendError { kind, ... })`. No new top-level error variants introduced by this RFC except the default-impl `Unavailable { op: &'static str }`.
- **No lifetimes on input types.** `&CreateFlowArgs` is acceptable; `&'a CreateFlowArgs<'a>` is not. Keeps the trait mockable in test code without HRTB gymnastics.
- **`async fn` everywhere.** The trait uses `#[async_trait]` in-tree today; RFC-017 does not switch to native `-> impl Future` (separate decision, not in scope).

### 5.1.1 Struct discipline: `#[non_exhaustive]` + constructors (round-1 F2)

Per project memory `non_exhaustive_needs_constructor` + `feedback_non_exhaustive_needs_constructor`: every public struct used as a trait argument or return value in RFC-017 MUST be `#[non_exhaustive]` AND MUST ship with a `pub fn new(required_fields...) -> Self` constructor in the same PR. The two requirements are paired — one without the other produces either an unbuildable dead API (attribute + no constructor) or a consumer-breakage cliff on every future field addition (constructor + no attribute).

**Structs newly introduced by RFC-017 (all get `#[non_exhaustive]` + `new`):**

- Args: `CreateExecutionArgs`, `CreateFlowArgs`, `AddExecutionToFlowArgs`, `StageDependencyEdgeArgs`, `ApplyDependencyToChildArgs`, `CancelExecutionArgs`, `ChangePriorityArgs`, `ReplayArgs`, `RevokeLeaseArgs`, `CreateBudgetArgs`, `ResetBudgetArgs`, `CreateQuotaPolicyArgs`, `ReportUsageAdminArgs`, `ListPendingWaitpointsArgs`, `ClaimForWorkerArgs`.
- Results: `CreateExecutionResult`, `CreateFlowResult`, `AddExecutionToFlowResult`, `StageDependencyEdgeResult`, `ApplyDependencyToChildResult`, `CancelExecutionResult`, `ChangePriorityResult`, `ReplayResult`, `RevokeLeaseResult`, `CreateBudgetResult`, `ResetBudgetResult`, `ReportUsageResult`, `ListPendingWaitpointsResult`, `ClaimForWorkerOutcome`, `BudgetStatus`, `PendingWaitpointInfo` (reshape per §8).

**Existing structs upgraded to `#[non_exhaustive]` + `new` at Stage A (v0.8 is a semver break anyway, so the upgrade is free):**

- `CreateExecutionArgs` (currently at `contracts/mod.rs:21`), `CancelExecutionArgs` (:336), `RevokeLeaseArgs` (:364), `CreateBudgetArgs` (:1309), `CreateFlowArgs` (:1511), `AddExecutionToFlowArgs` (:1529), `StageDependencyEdgeArgs` (:1613) — K F2 enumerated these; all confirmed on `origin/main`.

**Explicit non-goal:** RFC-017 does NOT introduce a `*Builder` pattern. The `pub fn new(required_fields...) -> Self` constructor + `pub fn with_<optional>(mut self, v: T) -> Self` fluent setters are the idiomatic pattern in this repo; see ferriskey `ClientBuilder` for precedent. This is explicitly called out so reviewers don't block on missing "builder" types.

### 5.2 Interaction with feature flags

`EngineBackend` today has three feature flags controlling method availability: `core`, `streaming`, and `admin`. RFC-017 adds methods into existing families:

- `core` continues to gate the bulk of the trait; the 12 new data-plane methods + `claim_for_worker` + `ping` + `revoke_lease` live under `core`.
- `streaming` continues to gate `read_stream` / `tail_stream` / `read_summary`; no change.
- `admin` gates `rotate_waitpoint_hmac_secret_all` (existing) and picks up `report_usage_admin` (new).
- **`backend_label` and `shutdown_prepare` are unconditional** — no feature gate — so any crate depending on `ff-core` sees them regardless of feature selection. This is deliberate per `feedback_feature_flag_propagation`: observability + shutdown are cross-cutting and must not silently disappear under narrower feature selections.

### 5.3 Postgres impl debt at the end of Stage A

After Stage A, `PostgresBackend` has impls for the 5 ingress methods (promoted from inherent), default-impl `Unavailable` for the 7 other new data-plane methods, and `"postgres"` for `backend_label`. That debt is paid down during Stage D (Postgres HTTP parity hardening). It is tracked explicitly in the `POSTGRES_PARITY_MATRIX.md` alongside the Stage D PR — any method still on `Unavailable` at Stage D ship time is a **hard block**, not a follow-up.

**Default impls.** All newly added data-plane methods get a default impl returning `EngineError::Unavailable { op: "<method_name>" }` — same pattern RFC-012 used for the initial trait landing. Each concrete backend overrides what it actually implements. This keeps Stage A additive: landing the trait expansion does not force Postgres to land all impls in one go.

**`shutdown_prepare` and `backend_label` are explicitly NOT defaulted** (post-round-1 F12 revision): every concrete backend specs its own. Rationale below (§5.4).

### 5.4 `shutdown_prepare` spec (round-1 F12)

**Signature:** `async fn shutdown_prepare(&self, grace: Duration) -> Result<(), EngineError>;` — no default impl.

Per-backend contract:

- **`ValkeyBackend`:** closes `stream_semaphore` (`Semaphore::close()`); awaits in-flight permit drops up to `grace`; drops `tail_client` handle. New `acquire_owned` calls return `EngineError::Unavailable { op: "tail_stream" }` immediately. In-flight calls complete normally. If `grace` elapses before drain, returns `EngineError::Timeout { op: "shutdown_prepare", elapsed: grace }`; caller (server) decides whether to hard-cancel or escalate.
- **`PostgresBackend`:** issues `UNLISTEN *` on the notification channel; drains the sqlx pool via `Pool::close_with_timeout(grace)`. In-flight queries complete up to `grace`; beyond that, they receive `Err(Canceled)`. No default no-op — the LISTEN session + pool lifecycle demand an explicit detach.
- **Future backends:** spec their own; the "no default" stance prevents silent wrong behaviour.

**Server integration:** `Server::shutdown()` calls `self.backend.shutdown_prepare(self.config.shutdown_grace).await` BEFORE draining `background_tasks`. Default `shutdown_grace = 30s`; configurable via `ServerConfig::shutdown_grace`. If `shutdown_prepare` returns `Timeout`, `Server` emits a `ff_shutdown_timeout_total` metric increment + `WARN` log + proceeds to background-task cancellation anyway (best-effort on bound).

**Test requirement (promoted to mandatory Stage B CI per round-1 F12):** `tests/shutdown_prepare_under_load.rs` opens 16 concurrent `tail_attempt_stream` sessions, triggers `Server::shutdown()` with `grace = 5s`, asserts (a) all 16 terminate cleanly within 5s; (b) no panics from dropped-permit accounting; (c) new `tail_attempt_stream` requests between `close()` and full drain get `HTTP 503` (mapped from `EngineError::Unavailable`), not `HTTP 500`.

---

## 6. Valkey-primitive encapsulation

Three `Server` fields are Valkey-artifact-only; all three move into `ff-backend-valkey::ValkeyBackend`:

```rust
pub struct ValkeyBackend {
    // existing
    client: Client,
    partition_config: PartitionConfig,
    // NEW — moved from Server
    tail_client: Client,
    stream_semaphore: Arc<Semaphore>,
    xread_block_lock: Arc<Mutex<()>>,
    // NEW — moved from Server (§7)
    scheduler: Arc<ff_scheduler::Scheduler>,
    // NEW — moved from Server
    admin_rotate_fanout_concurrency: u32,   // BOOT_INIT_CONCURRENCY = 16
}
```

Contract mapping:

| Primitive | Location after RFC | Surfaced as |
|---|---|---|
| `tail_client` | private field of `ValkeyBackend` | Used internally by `tail_stream` + `read_stream` impls. Invisible to `Server`. |
| `stream_semaphore` | private field of `ValkeyBackend` | Denial surfaces as `EngineError::ResourceExhausted { pool: "stream_ops", max, retry_after_ms: Option<u32> }` (post-round-1 F7); `ServerError::from` maps to HTTP 429 with the `Retry-After` header populated from `retry_after_ms` when present. Preserves current 429-on-contention behaviour byte-for-byte and keeps the current retry-hint path that reads `stream_semaphore.available_permits()` — the Valkey impl populates `retry_after_ms` server-side so the mapping layer doesn't need access to the private semaphore. |
| `xread_block_lock` | private field of `ValkeyBackend` | Internal to the Valkey `tail_stream` impl. No trait surface. |
| `admin_rotate_semaphore` | **stays on `Server`** | Server-wide fairness gate; not backend-coupled; applies equally to Postgres. |

**Why not surface these as trait methods?** Their contracts are Valkey-specific. The semaphore semantics (429 via `try_acquire_owned`) and mutex semantics (FIFO-mux serialisation against ferriskey pipelining) have no Postgres analogue. Inventing a neutral `acquire_stream_permit() -> StreamPermit` trait method forces every Postgres call through a no-op permit dance. Instead: the backend's `tail_stream` impl owns its own back-pressure strategy; `EngineError::ResourceExhausted` is the *standardised* boundary for "pool saturated, try again."

**If a future backend needs its own gate:** fine. It instantiates one inside its own impl. The RFC does **not** prescribe a gate at the trait level.

---

## 7. Scheduler location

**Manager call (D4, from prompt): schedulers stay scoped to the owning backend crate; `Server` calls one trait method.**

Status today (v0.7, post-Wave 5b PR #246):

- `ff_scheduler::Scheduler` holds a rotation cursor + partition config; currently constructed by `ff-server` with `client.clone()`.
- `ff-backend-postgres::PostgresScheduler` is its own module with its own cursor (SQL-based).

Both schedulers are already backend-scoped. This RFC consolidates the dispatch:

1. `ValkeyBackend` takes ownership of `Arc<ff_scheduler::Scheduler>` as a field (constructed in `connect_with_metrics`).
2. `PostgresBackend` takes ownership of `Arc<PostgresScheduler>` as a field.
3. `EngineBackend::claim_for_worker(args) -> ClaimForWorkerOutcome` becomes the trait method; each concrete backend forwards to its own scheduler module.
4. `Server::scheduler: Arc<ff_scheduler::Scheduler>` **stays** as a field **only for claim-path dispatch**, and `Server::claim_for_worker` calls `self.backend.claim_for_worker(...)` (no direct scheduler access from `Server` on the data plane). The field is **removed at Stage D** when the dispatch rewire is complete (round-1 F8 + Q4 resolution; closes Q4). No Postgres-panic surface exists mid-migration because `FF_BACKEND=postgres` is hard-gated to Stage E per §9 (round-1 F9). The two existing debug endpoints (`/debug/scheduler`, `/debug/partition-assignment`) are Valkey-only today and remain Valkey-only — they move into the Valkey-specific `ValkeyBackend::debug_router()` sub-router exposed by the Valkey crate; `Server` mounts it when `backend_label() == "valkey"`. No trait pollution.

Rationale: cursor state is backend-native (Valkey uses co-located lane queues; Postgres uses `FOR UPDATE SKIP LOCKED`). Forcing a common `Scheduler` abstraction above the trait would either leak Valkey-rotation semantics into Postgres or pollute Valkey with SQL-transaction assumptions. Keep it backend-local; expose only the dispatch entrypoint.

---

## 8. `list_pending_waitpoints` — schema rewrite + HMAC redaction

**Reframed post-round-1 per K F5 + L-3: this is a schema rewrite, not a drop-in redaction. Call it what it is.**

Today (`origin/main:crates/ff-core/src/contracts/mod.rs:824-854`), `PendingWaitpointInfo` has 8 fields:

```
waitpoint_id, waitpoint_key, state: String, waitpoint_token, required_signal_names,
created_at, activated_at, expires_at
```

K observed that the earlier §8 draft dropped `waitpoint_key`, `state`, `required_signal_names`, `activated_at` — all operationally load-bearing. `required_signal_names` drives reviewer-UI filtering when multiple concurrent waitpoints exist on one execution; `waitpoint_key` is how reviewers correlate across lanes; `state` + `activated_at` are how reviewers distinguish "still pending" from "just activated, pending resume dispatch". Dropping them is a real UX break. Re-added below.

**Revised contract — additive redaction, not wholesale rewrite:**

```rust
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingWaitpointInfo {
    pub waitpoint_id: WaitpointId,
    pub waitpoint_key: String,                       // KEPT — reviewer lane correlation
    pub state: String,                                // KEPT — "pending" / "active" / "closed"
    pub required_signal_names: Vec<String>,           // KEPT — reviewer UI filter
    pub created_at: TimestampMs,
    pub activated_at: Option<TimestampMs>,            // KEPT — pending-vs-active distinction
    pub expires_at: Option<TimestampMs>,
    // NEW (additive):
    pub execution_id: ExecutionId,                    // surfaces the owning execution without a separate lookup
    pub token_kid: String,                            // HMAC key id — safe to expose
    pub token_fingerprint: String,                    // 8-byte hex prefix of the HMAC, audit-friendly, not replayable
    // DROPPED:
    //   waitpoint_token — this is the only field removed; replaced by (token_kid, token_fingerprint)
}

impl PendingWaitpointInfo {
    pub fn new(
        waitpoint_id: WaitpointId,
        waitpoint_key: String,
        state: String,
        created_at: TimestampMs,
        expires_at: Option<TimestampMs>,
        execution_id: ExecutionId,
        token_kid: String,
        token_fingerprint: String,
    ) -> Self { /* required_signal_names = vec![], activated_at = None by default */ }
}
```

Diff vs `origin/main`: **1 field dropped, 3 fields added, 6 fields kept.** Not a rewrite.

**Trait boundary:** the backend never emits the raw `waitpoint_token` across `Arc<dyn EngineBackend>`. Mocks + test harnesses see only `token_kid` + `token_fingerprint`.

**Wire-format v0.7.x → v0.8.0 migration (round-1 L-3 — pick functional-plus-warn, not empty-string):**

- **v0.7.x:** wire response keeps emitting `waitpoint_token` **populated with the real HMAC** (status quo for existing consumers) + emits `Deprecation: ff-017` response header + logs a server-side audit event per serving (`ff_pending_waitpoint_legacy_token_served_total`). Cairn + other consumers get a dashboard signal to migrate before v0.8.0.
- **v0.8.0:** `waitpoint_token` removed entirely from the wire response; consumers MUST have switched to `(token_kid, token_fingerprint)` + the separate token-fetch path.

This is the **only** wire-format change in this RFC. Schema break at v0.8.0 is real; the one-release functional-plus-warn window (not the earlier draft's empty-string middle-path) gives consumers an observable migration signal without silently breaking their resume flows.

Callers needing the raw token (post-v0.8.0) hit a separate path `/v1/waitpoints/{id}/token` with stricter auth — scoped to an RFC-017 follow-up, not landed here.

**Pagination (round-1 F3 + K §Scenarios 7):** `ListPendingWaitpointsArgs` carries an `after: Option<WaitpointId>` cursor + `limit: Option<u32>` (default 100, max 1000). Valkey impl continues to drive SSCAN cursor-style; Postgres impl uses `WHERE waitpoint_id > $after ORDER BY waitpoint_id LIMIT $limit`. Trait-level pagination is mandatory — a flow with 10k pending waitpoints cannot be served in one round-trip regardless of backend.

Release notes must call out the v0.8.0 wire-break + the v0.7.x audit-log signal explicitly.

---

## 9. Stage plan — CI-gate-aligned

Four stages + a v0.8 cleanup, each CI-green at the workspace boundary per `feedback_rfc_phases_vs_ci`.

### 9.0 Postgres hard-gate (round-1 F9 + L-1)

**`FF_BACKEND=postgres` refuses to boot during Stages A-D.** `Server::start_with_metrics` validates `backend.backend_label()` against `const BACKEND_STAGE_READY: &[&str] = &["valkey"]` through Stage D; `"postgres"` joins the list only when Stage E (v0.8.0 cleanup) lands. On boot with a gated backend, `Server::start` returns `ServerError::BackendNotReady { backend: "postgres", stage: "D" }` and exits with code 2.

Rationale: silent 501s on 7+ HTTP routes during Stages A-D (default `Unavailable` impls on Postgres for the new data-plane methods) is worse than a hard boot refusal. An operator who tries to stand up a Postgres ff-server mid-migration gets a clear error at startup rather than runtime 501s scattered across 7 routes.

**In-tree artifact:** `docs/POSTGRES_PARITY_MATRIX.md` lands at Stage A merge (NOT alongside Stage D). Per L-1: greppable by cairn-fabric runbooks — one row per trait method × backend × status (`impl` / `unavailable` / `stub`). Updated at every stage merge.

**Stage D gate (mandatory CI, per L-1):** `test_postgres_parity_no_unavailable` iterates every HTTP-exposed trait method and asserts that `PostgresBackend::<method>(...)` on a live Postgres fixture never returns `EngineError::Unavailable`. Failure blocks Stage D merge. Runs in the two-backend CI matrix.

**Stage E boundary:** `BACKEND_STAGE_READY` updated to `&["valkey", "postgres"]`; the `test_postgres_parity_no_unavailable` gate has already passed at Stage D merge; v0.8.0 ships with Postgres as a first-class backend.

**Fleet-wide cutover requirement (round-2 K-R2-N1).** `BACKEND_STAGE_READY` is compiled into the binary, therefore per-node. During a rolling Stage D → Stage E upgrade across multiple `ff-server` replicas pointed at a shared Postgres instance, a mixed fleet is possible: nodes on the Stage-D binary refuse to boot with `FF_BACKEND=postgres` (returning `BackendNotReady` and exiting 2), while nodes on the Stage-E binary serve successfully. From a load-balancer / deployment-controller perspective this looks like crashlooping Stage-D nodes vs serving Stage-E nodes, indistinguishable from a config error. All `ff-server` instances in a deployment MUST therefore be on the Stage-E binary before `FF_BACKEND=postgres` is set. Rolling-upgrade from Stage D → Stage E with Postgres enabled is unsupported — perform a valkey→valkey Stage D→E rolling upgrade first, then flip `FF_BACKEND=postgres` as a second rollout. This callout also appears in the v0.8.0 release notes and the `docs/POSTGRES_PARITY_MATRIX.md` header.

**Dev-mode override (round-2 L-R2-2).** For cairn-fabric + internal pilot work during Stages B-D, an explicit unsupported-dev override is available: setting both `FF_BACKEND_ACCEPT_UNREADY=1` and `FF_ENV=development` bypasses `BACKEND_STAGE_READY`. On boot the server emits a loud `WARN` log (`backend_unready_boot_override: backend=postgres stage=C`) and increments `ff_backend_unready_boot_total{backend, stage}`. Production refuses this combination: if `FF_ENV` is unset or set to any value other than `development`, `FF_BACKEND_ACCEPT_UNREADY=1` is ignored and the hard-gate still fires. Documented as dev/CI-only in `docs/POSTGRES_PARITY_MATRIX.md`; not covered by semver; may be tightened or removed at any stage.

### Stage A — Trait expansion + parity wiring (weeks 0-2)

- Add the 20 new methods to `EngineBackend` with default impls returning `EngineError::Unavailable { op }`.
- Add `shutdown_prepare` (default no-op) and `backend_label` (default `"unknown"`).
- Promote the 5 ingress methods from `PostgresBackend` inherent → `impl EngineBackend for PostgresBackend`. Remove `#[cfg(feature = "core")]` gates.
- Add `impl EngineBackend for ValkeyBackend` bodies for all 20 new methods. Bodies wrap existing `ff_script::functions::*` helpers — source-of-truth is still the FCALL helpers, **zero behaviour change**.
- `ValkeyBackend::backend_label()` → `"valkey"`; `PostgresBackend::backend_label()` → `"postgres"`.
- CI gate: **workspace `cargo test`** + `deploy-approval` example + cairn-fabric consumer CI all green. No `ff-server` handler changes yet.

**Deliverable:** trait-level parity between Valkey and Postgres. `Server` still Valkey-only.

### Stage B — Read + admin + stream handler migration (weeks 2-3)

- Migrate handlers 1 (`ping`), 2 (reads), 10 (streams), 11 (rotate).
- Move `tail_client`, `stream_semaphore`, `xread_block_lock` from `Server` into `ValkeyBackend`. Delete the three `Server` fields.
- Update `ServerError` mapping: `EngineError::ResourceExhausted { pool: "stream_ops", max }` → HTTP 429.
- `Server::shutdown` calls `self.backend.shutdown_prepare().await` before draining `background_tasks`.
- `ServerConfig` gains `backend: BackendConfig` (sum-typed); legacy Valkey flat fields (`host`, `port`, `tls`, `cluster`, `skip_library_load`) retained as `#[deprecated]` shims populating `BackendConfig::Valkey(...)` via `From`.
- Stage-B feature flag `ff-server/backend-abstraction` guards the migration for mid-stage revert capability.
- CI gate: workspace green; 429 semantics verified in existing `tests/stream_concurrency.rs`; rotate fan-out test passes.

**Deliverable:** ~6 handlers migrated; stream primitives inside backend.

### Stage C — Operator control + budget + scheduling migration (weeks 3-5)

- Migrate handlers 3 (operator control), 4 (signal), 7 (budget/quota), 9 (claim).
- Add `ScriptError → EngineError` conversion shims at the backend boundary (preserve `ServerError::Backend(BackendError)` surface).
- Delete `Server::fcall_with_reload` → moved onto `ValkeyBackend`.
- Delete now-orphaned FCALL parsing helpers inside `server.rs` whose callers are gone.
- `Server::scheduler` field retained (some remaining backend-agnostic metrics reads) but `claim_for_worker` dispatches via trait.
- CI gate: full HTTP matrix against Valkey (existing); `cargo test` workspace green.

**Deliverable:** 12+ handlers migrated.

### Stage D — Ingress + boot + Postgres HTTP cutover (weeks 5-9)

- Migrate handlers 5 (flow ingress), 6 (execution ingress), 8 (pending waitpoints).
- Move Valkey-specific boot (`ensure_library`, `validate_or_create_partition_config`, `initialize_waitpoint_hmac_secret`, lanes SADD, `verify_valkey_version`) from `Server::start_with_metrics` into `ValkeyBackend::connect_with_metrics`.
- Wire `BackendConfig::Postgres` into `Server::start_with_metrics`; `ServerConfig::from_env` reads `FF_BACKEND`. **`FF_BACKEND=postgres` remains hard-gated at startup (§9.0) until Stage E.** `FF_BACKEND=postgres` boots successfully only after `BACKEND_STAGE_READY` is updated in Stage E.
- Land `Server::start_with_backend(config, backend, metrics)` as the test-injection + future-embedded-user entry point (from B §5).
- Remove `Server::client()` accessor and `Server::client: Client` field.
- Remove Stage-B feature flag.
- Add HTTP integration test `tests/http_postgres_smoke.rs` — create flow → add 3 executions → stage 2 edges → apply deps → cancel flow → assert terminal state.
- Land the `list_pending_waitpoints` HMAC redaction with `Deprecation: ff-017` header on the legacy token field.
- Per `feedback_smoke_after_publish`: scratch-project smoke hits all 13 handler families against the published artifact before close.
- CI gate: two-backend matrix (Valkey + Postgres); full workspace green.

**Deliverable:** `Server` holds only `Arc<dyn EngineBackend>` for data access. `FF_BACKEND=postgres ff-server` still refuses to boot (hard-gate lifted only in Stage E).

### Stage E — v0.8.0 cleanup + Postgres enablement (weeks 10+)

- Remove deprecated `ServerConfig` flat fields.
- Remove legacy `waitpoint_token` wire field from `/v1/pending-waitpoints` response (§8).
- Remove `Server::scheduler` field (now unused after full trait-dispatch, per F8 + Q4).
- **Update `BACKEND_STAGE_READY` to `&["valkey", "postgres"]`** — `FF_BACKEND=postgres` boots successfully for the first time.
- Publish migration matrix to cairn peer team per `feedback_peer_team_boundaries`.
- CI gate: `test_postgres_parity_no_unavailable` must pass (already green from Stage D); full two-backend workspace + scratch-project smoke.

Stage A is load-bearing; B/C/D are mechanical if A is clean.

### 9.1 Per-stage risk matrix

| Stage | Primary risk | Mitigation | Rollback |
|---|---|---|---|
| A | Trait-lift misses a semantic of the existing Lua FCALL (e.g. idempotency TTL on `create_execution`; dim-count validation on `create_budget`) | Parity-matrix tests (§14.1) run 28 cases covering happy + one error path per new method before Stage B merges | Revert is a single commit; trait stays additive with `Unavailable` defaults, no other crate broken |
| B | Primitive relocation changes 429 timing or `XREAD BLOCK` serialisation | Existing `tests/stream_concurrency.rs` + new 429-mapping test run under `ff-server/backend-abstraction` feature flag; revert = flip flag | Feature flag stays OFF by default through first CI cycle; operator-visible behaviour identical |
| C | `report_usage_admin` shape disagreement; `replay_execution` variadic-KEYS misalignment | Owner sign-off on Q1 + Q3 (§12) before Stage C merges; parity tests cover both paths | Revert individual handlers; trait methods stay (unused is harmless) |
| D | Postgres boot ordering (schema migrations vs HMAC install) diverges from Valkey's (FUNCTION LOAD + lanes SADD) in ways that break HTTP smoke | Two-backend CI matrix; scratch-project smoke before close; boot ordering documented in each backend's `connect_with_metrics` doc-comment | Revert Postgres entry point wiring; fall back to Valkey-only `Server::start_with_metrics` for v0.7.x patch; v0.8.0 re-attempts |
| v0.8 | External consumer breakage on shim removal | 8-week deprecation window; release notes + migration guide + peer-team comms per `feedback_peer_team_boundaries` | None — by v0.8 the shims are gone by design; consumers who miss the window cut a v0.7.x branch |

### 9.3 Consumer migration cadence (round-1 L-2)

Explicit guidance for cairn-fabric + other external consumers, addressing L's concerns about v0.7.x patch-cadence churn:

| Stage | Wall-clock window | Cairn action | SDK mock-impl update | Dashboard advice |
|---|---|---|---|---|
| Baseline | pre-Stage A | stay on v0.7.0-baseline; no action | — | existing dashboards work |
| A | weeks 0-2 | optional adopt; trait surface grows additively | **update once** — add all 20 new methods with `Unavailable` defaults on the mock | `ff_backend_kind` label not yet present on all instances; DO NOT key selectors on it |
| B | weeks 2-3 | **no action required** — trait is stable through Stage D | — | — |
| C | weeks 3-5 | **no action required** | — | — |
| D | weeks 5-9 | optional adopt; Postgres still gated | — | `ff_backend_kind` now universally present; selectors safe |
| E / v0.8.0 | week 10+ | mandatory upgrade: remove legacy `ServerConfig` flat-field usage; update `waitpoint_token` consumers to `(token_kid, token_fingerprint)` + separate token-fetch path | — | — |

**Trait stability freeze during Stages B/C (commitment to consumers, tightened per round-2 L-R2-1):** no new trait methods land between Stage A ship and Stage D merge, and **trait method signatures are frozen** (no parameter reorders, no return-type shape changes, no new required methods). Additive Args/Result field growth remains permitted *only* via the `#[non_exhaustive]` + fluent-setter discipline spec'd in §5.1.1 — such additions are guaranteed not to force a cairn mock recompile (existing `::new(...)` constructor calls remain valid; `match` expressions on Result types are already guarded by `#[non_exhaustive]`). Any non-additive change — rename, field removal, required-method addition, return-type restructure — is **deferred to post-Stage-E (v0.9.0 or later)**. Cairn mock-impl churn is therefore bounded to **exactly one update at Stage A**; stages B-D require zero SDK action. If an implementation PR during Stage B-D discovers a genuine non-additive need, that discovery itself is a Stage-plan finding and escalates to the owner rather than silently slipping into the stage.

**Dual-version compat test (promoted to mandatory CI, per L-2):** `tests/http_compat_cross_stage.rs` runs a golden-HTTP-diff suite asserting that every non-migrated handler in stage N produces byte-identical HTTP responses to stage N-1 for a fixed scenario matrix. Guards against silent behaviour drift during the migration window.

### 9.2 Stage-boundary CI contract

Per `feedback_rfc_phases_vs_ci`: a stage is not "merged" until `cargo test --workspace` is green on the integration CI. Local acceptance gates (`cargo test -p ff-server`) are necessary but not sufficient. Specifically for this RFC:

- Stage A CI must include the cairn-fabric consumer pipeline (trait additions are ABI-visible via `ff-sdk` re-exports).
- Stage B CI must include the stream-concurrency-rejected metric scrape to confirm the 429 mapping is byte-identical.
- Stage C CI must include the full HTTP matrix under both `FF_BACKEND=valkey` and `FF_BACKEND=postgres`.
- Stage D CI must include the scratch-project smoke against the published artifact before the release PR closes.

---

## 10. Semver impact — v0.8.0

**Call: this is v0.8.0, not v0.7.x.**

Adopted from B §10.4 and A §6.1 (A was loose; B was explicit).

Reasoning:

1. **Trait expansion by +20 methods breaks every external `impl EngineBackend for X`.** Any third-party backend (test mock crates in consumer codebases, hypothetical SQLite/DynamoDB adapters) stops compiling until it adds the new methods. Default impls help — a consumer can rely on them — but the trait surface **has changed**, and that's a minor-version break under Rust's stability contract.

2. **`ServerConfig` shim removal (Stage D + v0.8 cleanup) breaks external construction.** Any consumer who set `ServerConfig { host: ..., port: ..., ... }` directly (not through `Default` + `..`) stops compiling when the flat fields are removed.

3. **`Server::client() -> &Client` removal (Stage D).** Any consumer holding a direct ferriskey handle for bypass ops breaks.

4. **`list_pending_waitpoints` HMAC redaction.** Wire-format change (sanitised response body). Semver-breaking at the HTTP layer even if the Rust API is extended additively.

5. **`EngineError::Unavailable { op }` surface.** New enum variant on an existing public error type → minor-version bump regardless.

Per Rust ecosystem convention and Cargo semver rules, any of these alone justifies v0.8.0. Combined, it is unambiguous.

**Cut timing:** Stage D merges into v0.7.x with deprecation shims; the v0.8.0 tag drops the shims. v0.7.x consumers see additive changes only; v0.8.0 consumers see the cleanup.

---

## 11. Backward compatibility on `Server::start`

Consolidated from A §6 + B §7.

**`Server::start(config: ServerConfig) -> Result<Self, ServerError>` keeps its signature through v0.8.** Internally it splits into two entry points:

```rust
pub async fn start(config: ServerConfig) -> Result<Self, ServerError> {
    let metrics = Arc::new(ff_observability::Metrics::default());
    Self::start_with_metrics(config, metrics).await
}

pub async fn start_with_metrics(
    config: ServerConfig,
    metrics: Arc<ff_observability::Metrics>,
) -> Result<Self, ServerError> {
    // Post-round-1 F10: partition_config / lanes / waitpoint_hmac_secret move
    // INTO BackendConfig::Valkey; Server no longer threads them separately.
    let backend: Arc<dyn EngineBackend> = match &config.backend {
        BackendConfig::Valkey(v) => ValkeyBackend::connect_with_metrics(
            v.clone(),
            metrics.clone(),
        ).await?,
        BackendConfig::Postgres(p) => PostgresBackend::connect_with_metrics(
            p.clone(),
            metrics.clone(),
        ).await?,
    };
    // Post-round-1 F9: FF_BACKEND=postgres hard-gate until Stage E.
    if !BACKEND_STAGE_READY.contains(&backend.backend_label()) {
        return Err(ServerError::BackendNotReady {
            backend: backend.backend_label(),
            stage: env!("CARGO_PKG_VERSION"),
        });
    }
    Self::start_with_backend(config, backend, metrics).await
}

pub async fn start_with_backend(
    config: ServerConfig,
    backend: Arc<dyn EngineBackend>,
    metrics: Arc<ff_observability::Metrics>,
) -> Result<Self, ServerError> {
    // backend-agnostic setup only — engine, scheduler-field bootstrap, background tasks
}
```

`start_with_backend` is the **new test-injection + embedded-user entry point**. `MockBackend` (§14) plugs in here without touching `ServerConfig` construction.

**`partition_config` + `lanes` + `waitpoint_hmac_secret` ownership (round-1 F10).** These three fields move from `ServerConfig` into `BackendConfig::Valkey(ValkeyConfig { partition_config, lanes, waitpoint_hmac_secret, ... })` at v0.8.0. Rationale: they are Valkey-native constructs (partition hashing model + lanes SADD seed + per-partition HMAC secret), not generic server config. Postgres-backed deploys never set them.

v0.7.x shim: `ServerConfig` retains them as `#[deprecated]` top-level fields that populate `BackendConfig::Valkey(...)` through `From<LegacyServerConfig>`; a runtime **validation** asserts that `ServerConfig.partition_config` (legacy) matches `BackendConfig::Valkey.partition_config` (new) when both are set. Mismatch → `ServerError::ConfigConflict { field: "partition_config" }` at startup (not silent drift).

`start_with_backend` therefore takes its Valkey config *through the backend's own constructor*, not through `ServerConfig`. When a test wires `start_with_backend(config, Arc::new(MockBackend::new()), metrics)`, the mock's own constructor owns its partition_config; `ServerConfig` is consulted only for backend-agnostic fields (`http_addr`, `shutdown_grace`, etc.). No partition-count mismatch window.

`ServerConfig` evolution:

- **v0.7.x (Stages B-D):** `ServerConfig.backend: BackendConfig` lands. Legacy flat fields (`host`, `port`, `tls`, `cluster`, `skip_library_load`) stay `#[deprecated]`. `From<LegacyServerConfig> for ServerConfig` converts. `ServerConfig::from_env` reads `FF_BACKEND` (default `valkey` for parity) and falls back to legacy env vars.
- **v0.8.0:** flat fields removed.

Shim lifetime: **one minor release** (A Q-E leaned 1; B's plan confirmed 1). Rationale: `ff-server` has one in-tree consumer (cairn-fabric via HTTP, so no source change required) and the `deploy-approval` example (via ff-sdk, not ff-server). The external consumer pool using `ServerConfig` directly is small.

Wire format: **unchanged** across all routes. The only wire change is the `list_pending_waitpoints` HMAC redaction (§8), which ships in Stage D with a deprecation header and a v0.8.0 removal.

Metrics + tracing: `/metrics` scrape output preserved. Backend-specific series (ferriskey RTT histograms; sqlx pool metrics) emit per deployment; scrapers see a union. `backend_label()` enables a new `ff_backend_kind{backend="postgres"|"valkey"}` labelled counter (additive).

---

## 12. Open questions (owner-scoped)

**Status as of owner adjudication 2026-04-23: 0 open questions.** All five Qs (Q1–Q5) resolved; Q2 was the sole remaining owner-scoped question after round-1 and is now adjudicated (see below). Resolved during consolidation + round-1 revisions — **not** deferred: trait count (§2.3), `shutdown_prepare` (D2 + §5.4), `backend_label` (D3), HMAC exposure + schema (§8), semver (§10), config shim cadence (§11), scheduler location (§7), boot trait vs in-backend (D7), Postgres hard-gate (§9.0), struct discipline (§5.1.1), Retry-After wiring (§6).

**Closed in round-1** (previously open, now decided — see `round-1/author-response.md`):

- **Q1 (report_usage_admin shape)** — CLOSED in favour of split (smallest blast radius; `Handle` sum-type pollutes every worker call-site with an unwrap). Not owner-scoped per K F11.
- **Q3 (get_execution_result vs extending ExecutionSnapshot)** — CLOSED in favour of separate method. Binary payloads don't belong on a snapshot struct (§13.3 already takes this position). Not owner-scoped per K F11.
- **Q4 (Server::scheduler field retirement)** — CLOSED at Stage D per §7 + F8 resolution.
- **Q5 (third-party trait stability)** — CLOSED. Once v0.8.0 ships, `EngineBackend` is a public API under SemVer; stability is automatic. Sealing is out of scope for RFC-017 (separate RFC-012 R8+ question). Not owner-scoped per K F11.

**Remaining for owner:** *(none — all Qs adjudicated as of 2026-04-23)*

**Adjudicated in owner review (2026-04-23):**

1. **Q2 — `cancel_flow` dispatch ownership. ADJUDICATED → header-only on the trait + caller-side async polling.** Owner ruling (2026-04-23): "header-only + async cancel on the caller side. Consumer wanting sync-cancel semantics must explicitly await terminal state via `describe_flow` polling. Honest semantic: `cancel_flow` Ok-return means marked-for-cancel, not members-have-stopped. Side-effects window is real and must be acknowledged, not hidden." Rationale: header-only is consistent with v0.7's per-hop-tx discipline (no long-running transactions holding partition resources), avoids long-tx pathologies under Postgres, and is honest about the in-flight side-effect window rather than pretending cross-backend atomicity that Valkey cannot uniformly provide. §16 `cancel_flow` paragraph is rewritten below as the final single-behavior spec (not two options).

---

## 13. Alternatives rejected

### 13.1 "Keep `client: Client` on Server, add `backend: Arc<dyn EngineBackend>` alongside" (A §8.1)

Rejected. Doubles the boot cost (two Valkey connections + `tail_client`); invites drift where trait-migrated handlers and legacy handlers observe different state; creates cleanup debt. The staged cutover in §9 is cleaner.

### 13.2 Unify `ExecutionInfo` and `ExecutionSnapshot` (A §8.2)

Rejected for this RFC. Collapsing them is its own refactor with wire-format fallout. Handler #20 wraps `ExecutionSnapshot → ExecutionInfo` server-side; same wire output.

### 13.3 Narrow read methods (`get_state`, `get_result_bytes`) instead of reusing `describe_execution` (A §8.3)

Rejected. `get_state` via `describe_execution` costs 7 wasted HGET fields per call — <1ms overhead, not worth the trait-surface bloat (16 → 25+ redundant methods). `get_result` is separate (§12 Q3) only because binary payloads don't fit the snapshot struct; it is *not* a generic read-narrowing exception.

### 13.4 `Server<B: EngineBackend>` generic parameter (A §8.4, B §4.2)

Rejected. Monomorphisation duplicates the 1.5k-line server per backend. `axum`'s `State<Arc<Server>>` extractor doesn't care, and a future heterogeneous-backend binary becomes generic hell. `Arc<dyn>` is the right answer; vtable overhead is one pointer-chase, dwarfed by ferriskey RTT.

### 13.5 Backend enum `Backend { Valkey(...), Postgres(...) }` (A §3.1(d), B §4.1)

Rejected. Every handler becomes a `match` with 2+ arms of identical delegation. Third-party backends can't extend an in-tree enum without cross-crate dependency cycles. Match dispatch is not faster than vtable dispatch at `ff-server` call rates.

### 13.6 `Any`-downcasting from `Arc<dyn EngineBackend>` to `ValkeyBackend` for "extra" methods (B §3.2)

Rejected. Defeats mockability; hides backend coupling in string-literal form; Postgres-backed ff-server silently 500s on the downcast path instead of compile-erroring. **If a method is on the trait, every backend implements it. If a method isn't on the trait, no consumer calls it.**

### 13.7 Multi-trait split (`IngressBackend` + `AdminBackend` + ...) (A §3.1(c), B §3.1)

Rejected. Three `Arc<dyn>` fields on `Server`, all pointing at the same `Arc<ValkeyBackend>`. Trait-coherence burden with zero motivating use case. If a future need emerges, splitting later is mechanical (default-forwarding methods).

### 13.8 Companion `BackendBoot` trait (B §5)

Rejected during consolidation (D7). B admitted in §5.1 that the trait is "convention, not load-bearing." With one impl per backend and both constructed through the `BackendConfig` enum match in `Server::start_with_metrics`, a trait adds indirection without contract value. Boot lives inside `ValkeyBackend::connect_with_metrics` / `PostgresBackend::connect_with_metrics` — same logical shape, one fewer trait.

### 13.9 Delete `ff-server` and rely on consumers using `ff-sdk` directly (A §8.5)

Rejected. `ff-server` exists because HTTP is a first-class consumer surface (cairn-fabric, operator tooling, out-of-process clients). Killing it pushes HTTP into every consumer's codebase.

---

## 14. Testing plan

Consolidated from A §9 + B §13.

### 14.1 Stage A — trait parity

- **Parity matrix test per new trait method** (`crates/ff-backend-valkey/tests/parity_stage_a.rs`). Invoke `Server::<method>` (existing Valkey-direct path) and `self.backend.<new_method>()` against the same Valkey instance; assert identical observable state transitions. Target: 20 new methods × 1 happy-path + 1 error-path = 40 tests minimum.
- **Postgres inherent-to-trait promotion.** Existing `crates/ff-backend-postgres/tests/integration_flow_staging.rs` re-targets to the trait call.
- **`shutdown_prepare` + `backend_label` smoke.** `assert_eq!(valkey.backend_label(), "valkey"); assert!(valkey.shutdown_prepare().await.is_ok());` Same for Postgres.

### 14.2 Stage B — read + admin + stream

- **HTTP smoke against Postgres** for the 6 migrated routes. `FF_BACKEND=postgres cargo test -p ff-server -- http_postgres_read_smoke`.
- **Stream pool migration.** `tail_attempt_stream` 429-on-contention behaviour preserved via `EngineError::ResourceExhausted → HTTP 429` mapping.
- **Rotate fan-out.** Per-partition failure surfacing preserved. Existing HMAC-rotate test re-runs unchanged.
- **`shutdown_prepare` integration.** `Server::shutdown()` test asserts the semaphore closes and new stream requests get `EngineError::Unavailable` before the server exits.

### 14.3 Stage C — operator control + budget

- **Full HTTP matrix against both backends.** Every migrated route × (Valkey, Postgres) × (happy, one error path). ≈24 tests.
- **`cancel_flow` header-only + caller-side polling (per Q2 adjudication).** Stage C adds `tests/cancel_flow_poll_idiom.rs`: call `backend.cancel_flow(...)`; assert Ok returns promptly (sub-100ms under unloaded test harness); poll `backend.describe_flow(...)` and assert `public_flow_state` transitions to `Cancelled` within the dispatcher-tick budget + reconciler backstop window. Run against both Valkey and Postgres. Also asserts: Ok-return does NOT imply members-stopped (a test fixture with a deliberately slow member confirms the side-effects-window caveat is observable, not hidden). Existing server-local `cancel_flow_wait` convenience tests continue to pass (now implemented as poll-loop against the trait).
- **`report_usage_admin` parity.** Admin-path HTTP call vs worker-path SDK call produce identical budget state.

### 14.4 Stage D — ingress + boot + cutover

- **HTTP-Postgres end-to-end smoke** (`tests/http_postgres_smoke.rs`) — create flow → add 3 executions → stage 2 edges → apply deps → cancel flow → assert terminal state. **Gates v0.8.0 ship.**
- **Boot-path divergence test.** `ValkeyBackend::connect` runs library-load + HMAC install; `PostgresBackend::connect` runs migrations check. Each backend's harness exercises its own path.
- **Config shim test.** Legacy `ServerConfig { host: "x", port: 6379, ..., backend: None }` still constructs via the shim; v0.8.0 removes it.
- **HMAC redaction.** `list_pending_waitpoints` response body asserts `hmac_token == ""` + `Deprecation: ff-017` response header.

### 14.5 `MockBackend` ergonomics

Per B §13 — the single biggest ergonomic win from this RFC. `MockBackend` lives in `ff-server`'s test module; implements every `EngineBackend` method via `Arc<Mutex<ScriptedResponses>>`; handler tests run **without a Valkey container**. Ships in Stage A alongside the trait expansion.

### 14.6 Consumer regression

- **cairn-fabric CI** runs its full suite against each stage's `ff-server` build.
- **`deploy-approval` example** runs unchanged (consumes ff-sdk, not ff-server) as a canary for trait-level breakage.
- **Scratch-project smoke after publish** (per `feedback_smoke_after_publish`) hits all 13 handler families against the published `ff-server` + `ff-backend-postgres` artifacts before closing the v0.8.0 release.

### 14.7 Observability regression

- `/metrics` scrape before/after each stage — existing series set identical (no incumbent series disappears). Additions: `ff_backend_kind{backend=...}` label on existing counters.
- `ff_stream_ops_concurrency_rejected` counter relocates from `Server` to `ValkeyBackend`; same name, same label, same scrape.

### 14.8 Fuzzing + property coverage + shutdown race (mandatory post-round-1 F12)

- **Round-trip args.** `CreateFlowArgs` / `AddExecutionToFlowArgs` / etc. serde round-trip via `proptest` with `arb` generators — catches trait-boundary serialisation drift when a consumer upgrades one crate but not the other. Property coverage, non-blocking.
- **Error-variant exhaustiveness.** A unit test iterates `EngineError::<variant>` and asserts `ServerError::from(...)` maps every variant to a distinct HTTP status — catches silent fall-throughs to 500. Property coverage, non-blocking.
- **`shutdown_prepare` under active traffic — MANDATORY Stage B CI (promoted from "awareness" per round-1 F12).** `tests/shutdown_prepare_under_load.rs` opens 16 concurrent `tail_attempt_stream` sessions, calls `Server::shutdown()` with `grace = 5s`, and asserts: (a) all 16 terminate cleanly within 5s; (b) no panics from dropped-permit accounting on `stream_semaphore`; (c) new `tail_attempt_stream` requests arriving between `Semaphore::close()` and full drain get `HTTP 503` (mapped from `EngineError::Unavailable`), not `HTTP 500`; (d) the `ff_shutdown_timeout_total` metric stays at 0 for the happy path and increments when `grace` is deliberately set below the drain time. Fails → Stage B merge blocks.

---

## 15. Summary of consolidation decisions

For reference by reviewers comparing this RFC to the A/B divergent drafts:

| Decision | Choice | Source |
|---|---|---|
| Trait scope | Single trait, **51 methods (+20 vs v0.7)** — post-round-1 reconciliation | A §3.1 + B §3.1 agree on single-trait; count reconciled in round-1 §2.3 |
| Server field shape | `Arc<dyn EngineBackend>`, no enum, no generic | A §3.2 + B §4 agree |
| Valkey primitives | Encapsulated in `ValkeyBackend` | A §3.2 + B §9 agree |
| `shutdown_prepare` | Yes, on the trait | **A** |
| `backend_label` | Yes, on the trait | **B** |
| HMAC exposure in `list_pending_waitpoints` | Sanitised `PendingWaitpointInfo`; wire-format field removed v0.8 | **B** (security concern upgraded) |
| Semver impact | v0.8.0 | **B** (explicit) |
| Back-compat entry point | `Server::start` preserved + new `start_with_backend` | **B** (second entry point for test injection) |
| CI-gate alignment | Each stage = CI-green workspace | **B** (explicit) |
| Scheduler location | Backend-scoped modules; `claim_for_worker` on trait | **Manager call** (prompt §3) |
| Boot trait (`BackendBoot`) | Rejected; boot lives in each backend's `connect_with_metrics` | **Consolidation call** — B admitted it was convention-only |
| Shim lifetime | 1 minor release (v0.7.x → v0.8.0) | A §6.1 + B §7 agree |
| Stream-ops capability handle on Server | Rejected (Author A's initial sketch; adopt A's revised position) | **A revised** |

**Round-1 revisions applied (2026-04-23):** addressed K's 12 findings + L's 5 deltas; see `rfcs/drafts/RFC-017-challenges/round-1/author-response.md` for the per-finding concede/argue-back record. Material changes: count reconciled to 51 methods (+20), `#[non_exhaustive]` discipline spec added (§5.1.1), `PendingWaitpointInfo` reframed as additive redaction preserving 6 existing fields (§8), `FF_BACKEND=postgres` hard-gated to Stage E (§9.0), `shutdown_prepare` spec'd per-backend with grace budget (§5.4), `Retry-After` added to `ResourceExhausted` (§6), `partition_config` ownership moved into `BackendConfig::Valkey` (§11), open questions reduced from 5 → 1 (Q2 remained as sole owner-scoped); cross-backend semantics appendix added (§16).

**Owner adjudication applied (2026-04-23):** Q2 resolved — header-only `cancel_flow` on the trait with caller-side async `describe_flow` polling for sync-cancel semantics; §16 `cancel_flow` paragraph rewritten as the single-behavior final spec (not two options); caller-expectations semantic ("Ok = marked-for-cancel, not members-have-stopped; side-effects window is real") is load-bearing and documented in §16. **Open questions: 0.**

No divergences remain unresolved. All owner-scoped questions in §12 are adjudicated (Q2 closed 2026-04-23 — header-only cancel_flow + caller-side polling). **RFC-017 is ACCEPTED.**

---

---

## 16. Per-op cross-backend operational semantics (round-1 L-4)

Single paragraph per op. SRE-facing — what does a runbook author need to know about how the same trait method behaves differently on Valkey vs Postgres?

**`rotate_waitpoint_hmac_secret_all`.** Valkey: per-partition FCALL fan-out with `admin_rotate_fanout_concurrency = 16` (server-wide admin semaphore serialises cluster-wide). Runs in **~seconds** for typical deployments; partial-failure is observable (N-1 of N partitions rotated, returned in `RotateResult.partitions_failed`). Postgres: single `UPDATE secrets SET ... WHERE active = true` inside a transaction; runs in **~milliseconds**; partial-failure does not exist (transactional commit or rollback). Audit event schema (unchanged across backends) includes `backend_label()` so post-incident forensics can distinguish the two.

**`cancel_flow` dispatch (adjudicated 2026-04-23 — header-only + caller-side async polling).** `EngineBackend::cancel_flow` is a **header-only** trait method: the implementation INSERTs cancel markers into the outbox (Valkey: per-partition FCALL writes the cancel marker onto the flow header + each member's outbox entry; Postgres: single transactional `INSERT INTO outbox_events (kind='cancel', target_execution_id, ...)` plus header `UPDATE flows SET public_flow_state='cancelling'`) and returns `Ok(())` once those markers are durably written. **Members transition to cancelled via the dispatcher cascade** (the dispatcher consumes the outbox markers on its next tick per partition) plus the **reconciler backstop** (periodic sweep catches any member whose marker was missed due to partition-level failure and retries the cancel).

The caller-side idiom for "wait until the flow and all its members are actually terminal" is:

```rust
// 1. Fire-and-return cancel:
backend.cancel_flow(&flow_id, reason).await?;   // Ok = marked-for-cancel, NOT members-stopped

// 2. Caller polls describe_flow until public_flow_state == cancelled:
loop {
    let snap = backend.describe_flow(&flow_id).await?;
    if snap.public_flow_state == FlowState::Cancelled { break; }
    tokio::time::sleep(Duration::from_millis(200)).await;
}
```

This is the expected idiom and SHOULD be documented in the ff-sdk + ff-server API docs; `ff-server` exposes `POST /v1/flows/{id}/cancel` (fire-and-return) + clients call `GET /v1/flows/{id}` to poll terminal state. `cancel_flow_wait` (if retained as a `ff-server` convenience handler) implements this loop server-side against its own `backend` reference — it is NOT a distinct trait method.

**Caller expectations (load-bearing semantic — do not paper over).** `cancel_flow` returning `Ok(())` **does not mean members have stopped running**. It means cancel markers are durably recorded in the outbox and members WILL transition to cancelled on the next dispatcher tick (+ reconciler backstop). **In-flight side effects — e.g., outbound HTTP calls to external systems an execution had already started — may complete after the `cancel_flow` Ok-return.** This window is real (bounded by dispatcher-tick-latency + longest in-flight side-effect duration, typically sub-second to seconds, worst-case minutes for long HTTP calls) and must be acknowledged in consumer code, not hidden. Callers needing synchronous "all members observably stopped" semantics must poll `describe_flow` per the idiom above; there is no trait method that promises stronger. This matches v0.7's per-hop-tx discipline (no long-running transactions) and is uniform across Valkey and Postgres backends.

Valkey operational notes: cancel-marker writes are per-partition FCALL — partial failure across partitions is observable (`CancelFlowResult.partitions_failed: Vec<PartitionId>`); reconciler retries on next sweep. Postgres operational notes: single-transaction marker write — partial failure does not exist at the marker layer; dispatcher cascade still takes observable time to materialise member-state transitions. Audit emits `backend_label()` on both paths.

**`shutdown_prepare` (round-2 L-R2-3).** Both backends implement `shutdown_prepare(grace: Duration) -> Result<(), EngineError>` per §5.4, but the operator-visible mechanics diverge and a SIGTERM at `grace=30s` is not the same event on the two backends. Valkey: drains the 16 tail sessions of §14.8 (`tail_client` per-partition) + closes the stream semaphore + cancels outstanding `XREAD BLOCK` leases; observable as a bounded decline in `ff_stream_ops_inflight` over `grace`. Postgres: issues `UNLISTEN *` on each LISTEN connection, waits for in-flight `NOTIFY`-driven waiters to complete (bounded by `grace`), and drains the connection pool; observable as `ff_pg_listen_connections_active` draining to zero. Same trait method, distinct runbook. Audit emits `backend_label()` so dashboards can correlate. Neither backend promises "all work committed at `grace` boundary" — both return on their respective drain-complete signal or on timeout, whichever fires first.

**`list_pending_waitpoints` pagination (round-2 L-R2-3).** `ListPendingWaitpointsArgs { after: Option<WaitpointId>, limit: Option<u32> }` is uniform on the trait, but cursor semantics under concurrent SADD/SREM activity differ. Valkey: SSCAN cursor is best-effort-stable — under concurrent waitpoint registration/removal the cursor may miss newly-added entries or double-count entries present at both scan start and end. Acceptable for reviewer-UI pagination (the UI tolerates transient double-display). Postgres: `WHERE waitpoint_id > $after ORDER BY waitpoint_id LIMIT $limit` inside a `REPEATABLE READ` transaction gives exactly-once pagination over the snapshot. Operators should expect visibly different churn behaviour in the reviewer UI during heavy concurrent waitpoint activity — Valkey occasionally flickers, Postgres is stable. Not a bug on either side; document in the reviewer-UI runbook.

**`replay_execution`.** Valkey: `ff_replay_execution` takes variadic KEYS based on inbound-edge count (up to flow's max fan-in); trait method `ReplayArgs` carries `edges: Vec<EdgeRef>` and the Valkey impl expands them into the KEYS array at FCALL-time. Variadic-KEYS contract hidden from `Server`. Postgres: single `INSERT ... SELECT` from `edges_table WHERE target = $execution_id` using SQL UNNEST; no variadic concern. Trait hides both shapes. Failure modes: Valkey can fail mid-fan-out with a partial replay; Postgres is transactional. Runbook must note this.

`backend_label()` is required in **every admin audit emit** (`rotate`, `cancel_flow`, `replay_execution`) and in error reporting to Sentry/equivalent. Guarantees cross-backend forensic correlation.

---

**End of RFC-017 consolidated master (round-1 revised).** ~1150 lines.

Consolidator notes:
- A and B drafts retained at `rfcs/drafts/RFC-017-ff-server-backend-abstraction-A.md` and `-B.md` as the divergent-author record. PRs #259 and #260 stay open but are superseded by this consolidated draft (manager will not merge them).
- This RFC is ready for owner review + team debate per `feedback_rfc_team_debate_protocol`.
