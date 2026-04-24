# K — round 1 challenge on RFC-017

Persona: correctness + schema + distributed invariants. Written before reading L.

## Bottom line

**DISSENT.** The consolidated master has real teeth compared to A/B, but several load-bearing claims do not match the code on `main`, and the `#[non_exhaustive]` discipline that has protected the trait so far is silently abandoned for the +14 methods. Those must be closed before ACCEPT.

## Per-section review (§1..§15)

| §  | Topic | Verdict | Notes |
|----|------|---------|-------|
| 1 | Motivation / scope | GREEN | Gap and framing are right. |
| 2 | Inventory + reconciliation | YELLOW | 23-call-site figure is verifiable; reconciliation math in §2.3 is still off by one (see F1). |
| 3 | Shape + resolution log | YELLOW | D4 hybrid is defensible; D7 "boot inside `connect_with_metrics`" understates the invariant gap (F6). |
| 4 | Per-handler migration table | RED | T/M/H grades cheat — `list_pending_waitpoints`, `cancel_flow`, `replay_execution`, `create_execution`, boot relocation all under-rated (F3). |
| 5 | New trait methods | RED | Zero mention of `#[non_exhaustive]` + constructor discipline on the new Args/Result types (F2). `report_usage_admin` slipped in as "+1 not in the 14" (F4). |
| 6 | Valkey primitive encapsulation | YELLOW | `ResourceExhausted { pool: "stream_ops", max }` is asserted byte-for-byte but the concrete wire mapping isn't pinned (F7). |
| 7 | Scheduler location | YELLOW | `claim_for_worker` on the trait is fine, but `Server::scheduler` field retention during Stage A/B creates a dual-dispatch window (F8). |
| 8 | HMAC redaction | RED | The proposed `PendingWaitpointInfo` is not an additive redaction — it is a wholesale public-struct rewrite that drops four fields currently consumed by cairn-fabric reviewers (F5). |
| 9 | Stage plan | YELLOW | CI-gate alignment words are right; Stage A leaves Postgres with 7 `Unavailable` defaults — that's a broken intermediate state by the "non_exhaustive needs constructor" analogy (F9). |
| 10 | Semver → v0.8.0 | GREEN | Reasoning is solid. |
| 11 | Back-compat on `start` | YELLOW | `start_with_backend` signature drops `ServerConfig.partition_config` / `.lanes` ownership question (F10). |
| 12 | Open questions | RED | Q1/Q3/Q5 are not "owner-scoped choices" — they have single technically-correct answers (F11). |
| 13 | Alternatives rejected | GREEN | Good coverage. |
| 14 | Testing plan | YELLOW | Parity matrix does not guard `shutdown_prepare` under load rollover (F12). |
| 15 | Consolidation summary | GREEN | Clean trail. |

## Specific findings

### F1 — Trait-count arithmetic still doesn't close

§2.3 says "12 new data-plane + admin + `shutdown_prepare` + `backend_label` = 14." §5 adds `report_usage_admin` and concludes "30 + 12 + shutdown + label + report_usage_admin = 45." The summary at the top of the RFC (line 16) says 44. §15 says "45 methods (+14)." 44 ≠ 45; +14 ≠ +15.

**Minimal change → ACCEPT:** pick one number. Either fold `report_usage_admin` onto the existing `report_usage` row and commit to 44 / +14, OR promote it to its own counted new method and commit to 45 / +15. Inconsistency in the spec that is supposed to gate a breaking trait bump is itself a defect.

### F2 — `#[non_exhaustive]` discipline abandoned for the new Args/Result types

Project memory `non_exhaustive_needs_constructor` plus `feedback_non_exhaustive_needs_constructor` are load-bearing here. Current contracts in `crates/ff-core/src/contracts/mod.rs`: **none** of `CreateExecutionArgs` (L21), `CancelExecutionArgs` (L336), `RevokeLeaseArgs` (L364), `CreateBudgetArgs` (L1309), `CreateFlowArgs` (L1511), `AddExecutionToFlowArgs` (L1529), `StageDependencyEdgeArgs` (L1613) carry `#[non_exhaustive]`. Yet §5.1 claims "allows additive field growth without trait-signature churn." That claim is false if the structs aren't `#[non_exhaustive]`: adding a field to `CreateExecutionArgs` post-v0.8 breaks every struct-literal constructor in consumer code. Worse — if we *add* `#[non_exhaustive]` without a builder (what the memory calls out), the types become unbuildable.

**Minimal change → ACCEPT:** §5 MUST add a subsection mandating: (a) every new Args struct is `#[non_exhaustive]` at landing; (b) every new Args struct ships with a `pub fn new(required_fields...) -> Self` constructor in the same PR; (c) existing Args structs that grow new fields in v0.8.0 are upgraded to `#[non_exhaustive]` + constructor in Stage A, not later. Without this, v0.8 locks in a cliff of breakage for every future field addition.

### F3 — T/M/H difficulty is dishonestly scored

- Row 1 `ping` = T. Agreed.
- Row 2 `get_execution_result` = M. Plausible but the current impl at server.rs:840 reads from a bare `GET <result key>` composed against `exec_core`; mirroring into Postgres requires a new result-row schema or large-object store. That's H, not M.
- Row 4 `create_execution` = M. The Lua side is FCALL; the trait-lift is mechanical. But the `dedup_ttl_ms = 86400000` literal "lifts to `CreateExecutionArgs::idempotency_ttl: Option<Duration>` (default 24h), zero wire impact" (§4 row 6). It is NOT zero wire impact: the current HTTP route has no `idempotency_ttl` query/body field; bolting it onto `CreateExecutionArgs` either leaks into the JSON deserializer (wire change) or silently drops (bug). Pick one and document. M → H.
- Row 5 `list_pending_waitpoints` = M in §4 row 8, H in §2.2. Which is it. Given the pipelined SSCAN + 2× HMGET plus the §8 sanitisation, it is H.
- Row 13 `shutdown_prepare` = T. False — see F12 below. H under active traffic.
- Boot relocation (row 12) = unscored. It is the highest-risk single change: `ensure_library`, `validate_or_create_partition_config`, `initialize_waitpoint_hmac_secret`, `lanes SADD seed`, `verify_valkey_version` all move; ordering matters (FUNCTION LOAD must precede lanes seed on a fresh cluster, else the SADD scripts don't resolve). Score it H and add an explicit ordering contract.

**Minimal change → ACCEPT:** revise §4 to an honest distribution, add a "boot relocation ordering" sub-row, and reconcile §4 vs §2.2 on `list_pending_waitpoints`.

### F4 — `report_usage_admin` is a quiet +1 that escapes the count

§5 introduces `report_usage_admin` in a prose paragraph, then says "treats it as part of the `report_usage` family migration in §4 row 7." This is exactly the pattern that produced cairn-fabric's P2 gap (per `project_cairn_ff_sdk_gaps`): public-visibility wrapper that isn't tracked as a first-class trait surface. The feature-flag section (§5.2) also places `report_usage_admin` under `admin`; the existing `report_usage` is under `core`. That means a `core`-only backend gets asymmetric budget plumbing.

**Minimal change → ACCEPT:** promote `report_usage_admin` to a first-class row in §5, count it in the trait total explicitly, and pick the feature-flag boundary deliberately. If the admin path really belongs under `admin`, then a worker that is also an operator (single-binary deployment) needs both features active — say so.

### F5 — `PendingWaitpointInfo` is not an additive redaction

Actual struct at `crates/ff-core/src/contracts/mod.rs:824-854`:
```
waitpoint_id, waitpoint_key, state, waitpoint_token,
required_signal_names, created_at, activated_at, expires_at
```
RFC §8 proposed shape:
```
waitpoint_id, execution_id, status: WaitpointStatus, created_at,
expires_at, token_kid, token_fingerprint
```
Diff: drops `waitpoint_key`, `state: String`, `required_signal_names`, `activated_at`, `waitpoint_token`; adds `execution_id`, `status: WaitpointStatus` (new type, doesn't exist today under that name — only `state: String` does), `token_kid`, `token_fingerprint`.

The RFC frames this as "HMAC redaction with one release deprecation window." In fact:
- `required_signal_names` is operationally load-bearing (review UI filters on it — noted in the current doc comment on the field).
- `state: String` vs `status: WaitpointStatus` is a type-compat break even if both serialize to the same JSON tag.
- `waitpoint_key` (the keyspace-scoped partition key) disappears entirely — how does the reviewer correlate across lanes without it?

Wire format claim ("v0.7.x keeps emitting legacy `hmac_token` field populated with empty string + `Deprecation: ff-017` header") ignores that four *other* fields are disappearing.

**Minimal change → ACCEPT:** either (a) the RFC commits to a strictly additive redaction — keep every existing field, zero `waitpoint_token`, add `token_kid`/`token_fingerprint`, upgrade `state` to `WaitpointStatus` via `#[serde(alias)]`; or (b) call the change what it is — a breaking schema rewrite — and dedicate a subsection to operator impact + a migration story for the pending-waitpoint reviewer UI. Don't hide it under the "redaction" framing.

### F6 — D7 resolution leaks an invariant

§3.3 D7 drops `BackendBoot` and puts boot inside each backend's `connect_with_metrics`. That's fine for the Valkey path (idempotent boot steps exist today) but the RFC never states what `connect_with_metrics` promises. Under a rolling deploy:
1. Old node holds `FUNCTION LOAD`ed Lua V1.
2. New node boots, `ensure_library` re-loads V2 (atomic on the cluster).
3. Mid-flight FCALL from old node against V2 library — does it resolve? The lua function identifier is stable; the body isn't. Today this race is mitigated by the `skip_library_load` config path + deliberate staging order. Moving boot into the backend without locking the ordering contract means two replicas can each think they are authoritative for FUNCTION LOAD.

**Minimal change → ACCEPT:** §3.3 D7 must specify: (a) `connect_with_metrics` is idempotent w.r.t. existing state, (b) `ensure_library` is guarded by the same `skip_library_load` semantic that exists today, (c) the boot ordering sequence is documented as a contract on the `EngineBackend` trait doc-comment (not just in each impl's doc-comment). Otherwise Stage D's "Postgres boot ordering … diverges" risk in §9.1 is understated.

### F7 — 429 mapping not pinned

§6 says `ResourceExhausted { pool: "stream_ops", max }` maps to HTTP 429. §14.7 says `ff_stream_ops_concurrency_rejected` relocates with the same scrape shape. What about the `Retry-After` header? The current 429 path on `Server` sets a per-request retry hint based on `stream_semaphore.available_permits()`. If `stream_semaphore` is private to `ValkeyBackend`, the mapping layer can't compute it.

**Minimal change → ACCEPT:** `EngineError::ResourceExhausted` needs to carry (or `pool`, `max`, `retry_after_ms: Option<u32>`) and `ServerError::from` needs to map `retry_after_ms` to the `Retry-After` response header. Otherwise Stage B silently degrades 429 UX.

### F8 — `Server::scheduler` during Stage A/B is a dual-dispatch window

§7: `Server::scheduler: Arc<ff_scheduler::Scheduler>` stays; `Server::claim_for_worker` dispatches via the trait. But the scheduler is also consumed by metrics readers on `Server` ("some remaining backend-agnostic metrics reads" — §9 Stage C). If the scheduler is now *owned* by `ValkeyBackend` (§6) and *also* held by `Server` via `Arc`, the two handles observe the same state — fine — but the Postgres deploy has `Arc<PostgresScheduler>` inside its backend and nothing on `Server`. So during Stage A/B, `Server::scheduler_metrics()` works on Valkey and panics on Postgres. Q4 in §12 asks "retire Stage D or v0.8.0" — the real question is whether Stage A/B should even keep the field.

**Minimal change → ACCEPT:** §7 must state which concrete reads keep `Server::scheduler` alive through Stage C, and add a `scheduler_metrics()` trait method (or drop the field immediately in Stage A and route metrics via `backend.*`). Don't punt to an open question.

### F9 — Stage A leaves Postgres with 7 `Unavailable` defaults

§5.3 admits "after Stage A, `PostgresBackend` has … default-impl `Unavailable` for the 7 other new data-plane methods." Per `feedback_non_exhaustive_needs_constructor`, a `#[non_exhaustive]` type with no constructor is an unbuildable dead API; by the same logic a trait with methods that always return `Unavailable` is a dead-dispatch surface that the type system can't tell you about. Stage A's CI gate passes, but the end state is: if someone wires `FF_BACKEND=postgres` between Stage A merge and Stage D merge, 7 HTTP routes 501 with no compile-time warning.

**Minimal change → ACCEPT:** Stage A must either (a) land all 7 Postgres impls in the same stage (the RFC can be re-scoped to treat Postgres parity as Stage-A-gate, not Stage-D-gate), or (b) guard `FF_BACKEND=postgres` behind an explicit `experimental = true` flag that `Server::start` validates against `backend.backend_label()` + a "known-ready" list. Silent 501s on half the API are worse than a compile error.

### F10 — `start_with_backend` signature elides partition + lanes config ownership

§11 sketch:
```
pub async fn start_with_backend(
    config: ServerConfig,
    backend: Arc<dyn EngineBackend>,
    metrics: Arc<ff_observability::Metrics>,
) -> Result<Self, ServerError>
```
But `ValkeyBackend::connect_with_metrics` in `start_with_metrics` takes `config.partition_config.clone()`, `config.lanes.clone()`, `config.waitpoint_hmac_secret.clone()`. When a test wires `start_with_backend` with a `MockBackend`, who owns `partition_config`? If `ServerConfig` still carries it, the mock + `ServerConfig` can disagree (e.g. partition count mismatch).

**Minimal change → ACCEPT:** §11 must state that `partition_config` + `lanes` + `waitpoint_hmac_secret` move from `ServerConfig` into the backend's own config (Valkey variant), and `Server` reads them via `backend.partition_config()` / `backend.lanes()`. That's two more trait methods — count them.

### F11 — Open questions aren't open

Q1 (`report_usage_admin` shape): the consolidated lean is already correct — split. The `Handle` sum-type alternative pollutes every worker call-site with an unwrap. Not owner-scoped; it's a typed-over-untyped decision already settled by RFC-012 R7 precedent. **Close it.**

Q3 (`get_execution_result` vs extending `ExecutionSnapshot`): binary blobs don't belong on snapshot structs — §13.3 says so. The RFC already picked the answer in §5. **Close it.**

Q5 (trait stability for third-party impls): this is not a design choice. Once v0.8.0 ships, `EngineBackend` is a public API under SemVer; stability is automatic. The real question is whether we `#[sealed]` it (RFC-012 discussion). Unless RFC-017 proposes sealing, there is nothing for the owner to decide. **Close it.**

Q2 (`cancel_flow` dispatch ownership) and Q4 (scheduler field retirement timing) are legitimate owner calls. Keep those two; drop the rest.

**Minimal change → ACCEPT:** shrink §12 to Q2 + Q4.

### F12 — `shutdown_prepare` under active traffic is H, not T

§14.8 already flags "`shutdown_prepare` under active traffic … specifically guards against the Valkey semaphore close vs in-flight `acquire` race." The migration table in §4 row 13 scores this T. The race is:
1. Request A calls `tail_attempt_stream` → acquires `stream_semaphore` permit.
2. Operator issues `SIGTERM` → `Server::shutdown()` calls `backend.shutdown_prepare()`.
3. `shutdown_prepare` closes the semaphore; Request B sees `acquire` error.
4. Request A completes, drops the permit. Semaphore has one free slot but is closed — is the drop observable? Does Request A's response include the `Connection: close` hint?

`tokio::sync::Semaphore::close()` is irreversible; permits in-flight return normally on drop but new `acquire_owned` returns `Closed`. The RFC's default impl `Ok(())` on non-Valkey backends papers over this. Postgres's LISTEN session needs an explicit `UNLISTEN *` + pool drain, which can block on in-flight queries — default no-op is *wrong* for Postgres.

**Minimal change → ACCEPT:** (a) score it H; (b) `shutdown_prepare` takes a `Duration` grace period argument or the RFC explicitly defines "best-effort, server calls with its own timeout wrapper"; (c) Postgres impl is spec'd in §5 (not defaulted to no-op) with the `UNLISTEN` + pool drain step; (d) §14.8 moves from "called out for owner awareness" to a mandatory Stage B CI test.

## Scenarios the spec doesn't answer

1. **Rolling v0.7.x → v0.8.0 with two `ff-server` replicas.** Replica A has `ServerConfig { host, port, backend: BackendConfig::Valkey(...) }` (both legacy + new wired); replica B is pure v0.8.0 (flat fields gone). Operator config management renders the same TOML to both. Does the legacy-flat-field shim on A silently win over `backend: BackendConfig::Valkey(...)` when both are set, or does it panic on conflict? §11 doesn't say.

2. **`FF_BACKEND=postgres` during Stage B–C.** Per F9, 7 trait methods default to `Unavailable`. A cairn-fabric operator testing Postgres HTTP support hits 7 of 13 routes with 501. Is that documented in release notes? §9.1 Stage C mitigation doesn't mention it.

3. **Concurrent `rotate_waitpoint_hmac_secret_all` + handler migration.** An operator triggers rotate during Stage D's per-partition fan-out; does the `admin_rotate_semaphore` (which stays on `Server`) correctly serialize against a partial-migration state where some handlers call the trait and others still do direct FCALL? The RFC says "admin semaphore stays on Server" (§6) but doesn't explain what happens if the backend's own `rotate` impl is *also* counting something.

4. **`claim_for_worker` vs `cancel_flow` race under Postgres `FOR UPDATE SKIP LOCKED`.** §7 says Postgres uses SKIP LOCKED; §13 rejects a split scheduler trait. If `claim_for_worker` has an open SKIP LOCKED cursor and a `cancel_flow` admin call arrives for the same flow, the cancel's reconciliation path (Q2 unresolved) either blocks or races. Valkey behaviour is co-located and atomic via FCALL; Postgres is neither. The trait makes the behaviours look identical but the invariants diverge.

5. **`MockBackend` in handler tests (§14.5) — what does it mock for `shutdown_prepare`?** A handler test that calls `Server::shutdown()` against `MockBackend` is exercising the graceful-drain path. If the mock returns `Ok(())`, the test doesn't exercise the real semaphore race. §14.8's "concurrent tail_attempt_stream" integration test is mandatory — which backend does it run under? If only Valkey, the Postgres pool-drain path is untested.

6. **Stage D scratch-project smoke against published artifact.** `feedback_smoke_after_publish` says to run it before closing. Does the scratch project build against `FF_BACKEND=postgres` or `FF_BACKEND=valkey`? Both? If both, the v0.8.0 release gate doubles — and the Postgres path requires a running Postgres in CI scratch.

7. **`list_pending_waitpoints` pagination.** Current impl uses SSCAN; RFC doesn't say whether the trait method accepts a cursor/limit. For a flow with 10k pending waitpoints this is a latency cliff.

## Questions

- **Q-K1.** Are the new Args/Result structs `#[non_exhaustive]` at landing and paired with constructors, or not? (§5 is silent; memory rule is clear.)
- **Q-K2.** Does `PendingWaitpointInfo` redaction keep `required_signal_names`, `waitpoint_key`, `state`, `activated_at`, or drop them? (§8 implicitly drops; consumers break silently.)
- **Q-K3.** Why does §9 tolerate Postgres + HTTP serving 7 routes of 501 between Stage A and Stage D? What's the plan to prevent an operator from wiring `FF_BACKEND=postgres` during that window?
- **Q-K4.** `shutdown_prepare` with no timeout argument — under Postgres with an idle-in-transaction query, can it hang indefinitely? If yes, who aborts?
- **Q-K5.** `report_usage_admin` feature-flag placement — `admin` or `core`? One locks out single-binary deployments; the other leaks admin plumbing into `core`-minimal consumers.
- **Q-K6.** Trait total: 44 or 45? The RFC says both.
- **Q-K7.** Does `Server::start_with_backend` implicitly require `ServerConfig.partition_config` to match `backend.partition_config()`? If yes, what happens when they disagree — panic, error, silent mismatch?

---

Push branch + file, no PR. Awaiting L's position + author response.
