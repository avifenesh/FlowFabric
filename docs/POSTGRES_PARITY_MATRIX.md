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
| 1 | `create_execution` | `impl` | `stub` | Valkey wraps `ff_create_execution` FCALL. Postgres inherent `create_execution(CreateExecutionArgs) -> ExecutionId` already exists but return shape differs from trait — trait default returns `Unavailable` until Wave 4d reshapes. |
| 2 | `create_flow` | `impl` | `impl` | Promoted from PG inherent to trait. |
| 3 | `add_execution_to_flow` | `impl` | `impl` | Promoted from PG inherent to trait. |
| 4 | `stage_dependency_edge` | `impl` | `impl` | Promoted from PG inherent to trait. |
| 5 | `apply_dependency_to_child` | `impl` | `impl` | Promoted from PG inherent to trait. |
| 6 | `cancel_execution` | `stub` | `stub` | Valkey body requires HMGET pre-read (lane_id + current_attempt_index + current_waitpoint_id + current_worker_instance_id); deferred to Stage C handler migration to keep this PR surgical. |
| 7 | `change_priority` | `stub` | `stub` | Valkey body requires HGET lane_id pre-read; deferred to Stage C. |
| 8 | `replay_execution` | `stub` | `stub` | Valkey body requires HMGET + variadic SMEMBERS (§4 row 3 hard-level complexity); deferred to Stage C. |
| 9 | `revoke_lease` | `stub` | `stub` | Valkey body requires HGET current_worker_instance_id pre-read; deferred to Stage C. |
| 10 | `create_budget` | `impl` | `stub` | Valkey wraps `ff_create_budget`. Postgres default `Unavailable` until Wave 5 budget impls. |
| 11 | `reset_budget` | `impl` | `stub` | Valkey wraps `ff_reset_budget`. |
| 12 | `create_quota_policy` | `impl` | `stub` | Valkey wraps `ff_create_quota_policy`. |
| 13 | `get_budget_status` | `stub` | `stub` | Valkey body is 3× HGETALL + field-level parse (no FCALL); deferred to Stage C to keep the budget-shape refactor in one place. |
| 14 | `report_usage_admin` | `impl` | `stub` | Valkey wraps `ff_report_usage_and_check` without worker handle. |
| 15 | `get_execution_result` | `impl` | `stub` | Valkey direct `GET` of `ctx.result()`, binary-safe. |
| 16 | `list_pending_waitpoints` | `stub` | `stub` | §8 schema rewrite (HMAC redaction + `token_kid`/`token_fingerprint`) is Stage D's explicit scope. Stage A lands the trait signature only. |
| 17 | `ping` | `impl` | `impl` | Valkey: `PING`. Postgres: `SELECT 1`. |
| 18 | `claim_for_worker` | `stub` | `stub` | Valkey impl requires `Arc<ff_scheduler::Scheduler>` field on `ValkeyBackend` — adding `ff-scheduler` dep to `ff-backend-valkey` is a dep-graph change kept out of Stage A; scheduler lifts in Stage C / §7. |

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
- **Stage C (next):** operator control + budget-status + claim
  handler migration. Valkey `stub` rows above move to `impl`.
- **Stage D:** ingress + `list_pending_waitpoints` §8 schema rewrite
  + Postgres HTTP cutover. **CI gate:**
  `test_postgres_parity_no_unavailable` asserts no Postgres
  `Unavailable` on any HTTP-exposed method. Every `stub` in the
  Postgres column above must be `impl` before Stage D merges.
- **Stage E (v0.8.0):** `BACKEND_STAGE_READY` updated to
  `&["valkey", "postgres"]`; `FF_BACKEND=postgres` boots successfully
  for the first time.
