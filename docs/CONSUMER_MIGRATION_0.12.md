# Consumer migration — v0.11 → v0.12

**Scope.** v0.12 ships **RFC-023 — SQLite dev-only backend** plus a
small set of ff-server type-shape changes that fall out of wiring a
third backend. The release is mostly additive; the two breaking shape
adjustments are compile-time only and cluster on `ServerError` +
`ServerConfig`. Runtime behaviour under `FF_BACKEND=valkey` and
`FF_BACKEND=postgres` is unchanged from v0.11.

A full per-change listing lives in the `[0.12.0]` section of
`CHANGELOG.md`; this doc focuses on the operator + consumer code
checklist for adopting v0.12.

## What shipped

### 1. `ff-backend-sqlite` — new crate, dev-only (RFC-023)

A third `EngineBackend` implementation scoped **permanently** to the
dev-only / testing-harness role. See
[`docs/dev-harness.md`](dev-harness.md) for the canonical setup +
dev→prod gotchas, and
[`rfcs/RFC-023-sqlite-dev-only-backend.md`](../rfcs/RFC-023-sqlite-dev-only-backend.md)
for the design record.

Key points for consumers:

- **Positioning.** SQLite is a testing harness; Valkey is the engine;
  Postgres is the enterprise persistence layer. Pick Valkey or
  Postgres for production; pick SQLite for `cargo test` without
  Docker and contributor first-clone onboarding.
- **`FF_DEV_MODE=1` is required.** Both the ff-server branch and
  library-level `SqliteBackend::new` refuse to construct unless
  `FF_DEV_MODE` is set to `1`. This is a dev-only safety gate;
  production binaries that hit `FF_BACKEND=sqlite` without
  `FF_DEV_MODE=1` fail loudly rather than silently.
- **Capability parity.** `SqliteBackend::capabilities()` reports the
  same `Supports` flag set as Postgres at v0.11, with the documented
  exception of `claim_for_worker` (no scheduler is wired on SQLite —
  RFC-023 §5 non-goal) and `subscribe_instance_tags` (`n/a` on all
  three backends per #311). The full matrix is in
  [`docs/POSTGRES_PARITY_MATRIX.md`](POSTGRES_PARITY_MATRIX.md).
- **Embedded (cairn-canonical) path.** `SqliteBackend::new(path)` +
  `FlowFabricWorker::connect_with(config, backend, None)`. No HTTP
  listener, no `reqwest` dep. This is the shape cairn's `cargo test`
  uses. See [`docs/dev-harness.md`](dev-harness.md) for a working
  example.
- **HTTP path.** `ServerConfig::sqlite_dev()` builds a pre-wired
  `ServerConfig` (backend=Sqlite, `:memory:` path, auth disabled,
  `127.0.0.1:0` listen); pair with `Server::start_with_backend` to
  exercise the full REST surface.

### 2. `ff-sdk::FlowFabricWorker` re-export no longer `valkey-default`-gated

Pre-v0.12 the `worker` module and the `FlowFabricWorker` re-export
in `ff-sdk` were behind `#[cfg(feature = "valkey-default")]`.
Consumers using `ff-sdk = { default-features = false, features =
["sqlite"] }` could not name `FlowFabricWorker`.

At v0.12 the module + re-export are always compiled. The
ferriskey-dependent methods (`connect`, `claim_next`,
`claim_from_grant`, `claim_via_server`, `claim_from_reclaim_grant`,
`claim_resumed_execution`, `claim_execution`,
`read_execution_context`, `deliver_signal`) stay gated at the item
level and are absent under `--no-default-features, features =
["sqlite"]`. Under that feature set, `FlowFabricWorker` exposes
`connect_with`, `backend`, `completion_backend`, `config`, and
`partition_config`; consumers drive ops directly through the
`EngineBackend` trait via `worker.backend()`.

This is **additive under `valkey-default`** (the default feature set
every shipped consumer uses today) — no runtime behaviour change,
only that `FlowFabricWorker::connect_with` no longer fires the
`connect` preamble's throwaway Valkey round-trips. See RFC-023 §4.4
item 10 for the compile-surface details.

### 3. Minor breaking: `ServerError` + `ServerConfig` → `#[non_exhaustive]`

Adding `BackendKind::Sqlite` + `ServerConfig::sqlite:
SqliteServerConfig` + `ServerError::SqliteRequiresDevMode`
introduces a new enum variant and a new struct field. Both types
were not `#[non_exhaustive]` pre-v0.12, so:

- **Exhaustive `match` arms on `ServerError`** now need a wildcard
  arm (or must add `SqliteRequiresDevMode` explicitly).
- **`ServerConfig` struct-literal construction** breaks due to the
  new `sqlite` field. Migrate to `ServerConfig::from_env()`, the new
  `ServerConfig::sqlite_dev()` builder, or `..Default::default()`
  spread.

Both types are sealed with `#[non_exhaustive]` in the same PR so
future variant/field additions are additive under this RFC's
pre-1.0 posture.

### 4. New migrations — SQLite 0001–0014, Postgres 0015 + 0016

SQLite ships its own hand-ported migration set — migrations 0001
through 0014 in `crates/ff-backend-sqlite/migrations/` — 1:1 numbered
with Postgres for parity-drift detection. First-time SQLite users
need nothing extra; `SqliteBackend::new` applies migrations
idempotently on pool init (`sqlx::migrate!`).

Postgres migrations 0015 and 0016 are **additive, forward-only** and
land at v0.12 as Wave-9 follow-ups:

| # | Adds | Purpose |
|---|---|---|
| 0015 | *(reserved for RFC-024 claim-grant table; renumbers safely if RFC-024 lands first)* | — |
| 0016 | `ff_exec_core.started_at_ms` column | Set-once first-claim timestamp on the core row; drops the LATERAL / correlated subquery on `ff_attempt.started_at_ms` from the Wave-9 Spine-B read path. Backfilled from `MIN(ff_attempt.started_at_ms)` per execution at migration time. |

`ff-server` auto-runs `apply_migrations` at boot on the Postgres
path; operators managing schema out-of-band must apply 0016 before
rolling v0.12.0 binaries.

### 5. Cairn-canonical dev pattern (reference)

```toml
# cairn-fabric/Cargo.toml (test-only dep shape)
[dev-dependencies]
ff-sdk             = { version = "0.12", default-features = false, features = ["sqlite"] }
ff-backend-sqlite  = "0.12"
```

Set `FF_DEV_MODE=1` via `.cargo/config.toml [env]` (recommended —
see [`docs/dev-harness.md`](dev-harness.md)); `std::env::set_var`
is `unsafe` under Rust 2024 edition and racy across parallel
tests.

```rust
// cairn-fabric/tests/integration_sqlite.rs
use ff_sdk::{FlowFabricWorker, WorkerConfig, SqliteBackend};
use std::sync::Arc;

#[tokio::test]
async fn roundtrip_on_sqlite() {
    // FF_DEV_MODE=1 is set in .cargo/config.toml [env] — no set_var needed.

    let uri = format!(
        "file:cairn-test-{}?mode=memory&cache=shared",
        uuid::Uuid::new_v4(),
    );
    let backend = Arc::new(SqliteBackend::new(&uri).await.expect("sqlite init"));

    let config = WorkerConfig::builder()
        .lanes(vec!["default".into()])
        .build()
        .expect("worker config");
    let worker = FlowFabricWorker::connect_with(config, backend, None)
        .await
        .expect("worker connect");

    // ... your integration test logic, driving ops through `worker.backend()` ...
}
```

See [`docs/dev-harness.md`](dev-harness.md) for the canonical
`.cargo/config.toml [env]` setup that avoids `std::env::set_var` in
test bodies (unsafe under parallel test harness on Rust 2024).

### 6. New env vars

| Variable | Default | Purpose |
|---|---|---|
| `FF_BACKEND=sqlite` | `valkey` | Selects the SQLite backend (alongside `valkey` / `postgres`). |
| `FF_DEV_MODE` | *(unset)* | **Required** when `FF_BACKEND=sqlite` or when constructing `SqliteBackend::new` directly. No effect on other backends. |
| `FF_SQLITE_PATH` | `:memory:` | File path (`/tmp/ff-dev.db`) or URI (`file:name?mode=memory&cache=shared`). |
| `FF_SQLITE_POOL_SIZE` | `4` | Pool size (1 writer + N–1 readers). |

### 7. RFC-024 — lease-reclaim consumer surface

RFC-024 (accepted 2026-04-26) lands in v0.12.0 alongside RFC-023.
Closes the pull-mode deadlock at issue #371: consumers that drive
FlowFabric one HTTP request at a time (cairn-fabric's model) could
previously hit `lease_expired` on `POST /v1/runs/:id/complete` and
have NO recovery path — `ff_issue_claim_grant` rejected with
`execution_not_eligible` because the execution was in
`lifecycle_phase = active`, not `runnable`. RFC-024 adds a dedicated
admission path for the `lease_expired_reclaimable` / `lease_revoked`
states.

#### Breaking changes (pre-existing surfaces)

These landed in PR-B/PR-C and are already documented in
`CHANGELOG.md`:

- **`ReclaimGrant` renamed to `ResumeGrant`** (the pre-RFC type
  always represented the resume-after-suspend semantic). `cargo fix`
  handles most call-sites.
- **Trait method rename** `EngineBackend::claim_from_reclaim` →
  `EngineBackend::claim_from_resume_grant`. Matching SDK method
  rename on `FlowFabricWorker`.
- **`ReclaimExecutionArgs::max_reclaim_count`** type change:
  `u32` → `Option<u32>`. `None` ⇒ Rust-surface default of 1000
  (RFC-024 §4.6). Existing callers passing a value wrap in
  `Some(...)`; the Lua fallback of 100 still applies to pre-RFC
  call sites (wire-compatible under `#[serde(default)]`).
- **`#[non_exhaustive]`** added to `ClaimGrant`, `ResumeGrant`,
  new `ReclaimGrant`, `IssueReclaimGrantArgs`,
  `ReclaimExecutionArgs`. Each type gains an explicit `::new`
  constructor (per
  `feedback_non_exhaustive_needs_constructor`).
- **`HandleKind::Reclaimed`** variant added. `HandleKind` is already
  `#[non_exhaustive]`; consumers matching exhaustively add an arm
  (or rely on `_ =>` fallthrough).

#### Additive (PR-G — this release)

The three new consumer surfaces in v0.12.0:

- `FlowFabricAdminClient::issue_reclaim_grant(&self, execution_id,
  IssueReclaimGrantRequest) -> Result<IssueReclaimGrantResponse,
  SdkError>` — HTTP `POST /v1/executions/{id}/reclaim`. Admits the
  execution into the reclaim path; returns a `Granted` /
  `NotReclaimable` / `ReclaimCapExceeded` outcome.
- `FlowFabricWorker::claim_from_reclaim_grant(&self, ReclaimGrant,
  ReclaimExecutionArgs) -> Result<ReclaimExecutionOutcome,
  SdkError>` — backend-agnostic. Dispatches through
  `EngineBackend::reclaim_execution` on whichever backend the
  worker was connected with. NOT `valkey-default`-gated; compiles
  and runs under `--no-default-features, features = ["sqlite"]`.
- `POST /v1/executions/{id}/reclaim` HTTP endpoint on `ff-server`.
  Request body carries worker identity + lane + capabilities;
  response is a `status`-discriminated JSON outcome.

#### Admission model

Grant issuance rejects with
`IssueReclaimGrantResponse::NotReclaimable { detail }` when any of:

- The execution's `lifecycle_phase` is not `active`, or its
  `ownership_state` is neither `lease_expired_reclaimable` nor
  `lease_revoked` (detail carries the observed state).
- A capability is missing: the backend computes the set difference
  of `exec_core.required_capabilities` (populated from
  `ExecutionPolicy.routing_requirements.required_capabilities` at
  `create_execution` time) minus the request's
  `worker_capabilities`. A non-empty difference surfaces as
  `detail: "capability_mismatch: <missing csv>"`.

`capability_hash` on the request body is **not** consulted for
admission — it is an opaque audit token stored verbatim on the
grant hash. Set it to `None` unless you have a downstream
correlation use-case.

#### Cairn migration pattern — F64 bridge retry loop → reclaim

Pre-RFC cairn-fabric pattern at `cairn-rs/.../f64_bridge.rs`
(marked `remove once FF#371 ships`):

```rust
// BEFORE (v0.11): retry-with-backoff on lease_expired. Eventually
// fails because `ff_issue_claim_grant` gates on runnable phase.
loop {
    match worker.complete(handle, output).await {
        Err(SdkError::Engine(e)) if is_lease_expired(&e) => {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;  // retries until grant_ttl drifts out
        }
        other => return other,
    }
}
```

```rust
// AFTER (v0.12): one-shot reclaim on lease_expired.
use ff_sdk::admin::{FlowFabricAdminClient, IssueReclaimGrantRequest};
use ff_core::contracts::ReclaimExecutionArgs;

match worker.complete(handle, output).await {
    Err(SdkError::Engine(e)) if is_lease_expired(&e) => {
        // 1. Issue a reclaim grant via the admin HTTP surface.
        let req = IssueReclaimGrantRequest {
            worker_id: worker.config().worker_id.to_string(),
            worker_instance_id:
                worker.config().worker_instance_id.to_string(),
            lane_id: lane.to_string(),
            capability_hash: None,
            grant_ttl_ms: 30_000,
            route_snapshot_json: None,
            admission_summary: None,
            worker_capabilities: worker
                .config()
                .capabilities
                .clone(),
        };
        let resp = admin
            .issue_reclaim_grant(&execution_id.to_string(), req)
            .await?;
        let grant = resp.into_grant()?; // → SdkError on non-Granted

        // 2. Consume the grant to mint a fresh attempt.
        let args = ReclaimExecutionArgs::new(
            execution_id.clone(),
            worker.config().worker_id.clone(),
            worker.config().worker_instance_id.clone(),
            lane.clone(),
            None,                               // capability_hash
            new_lease_id,
            worker.config().lease_ttl_ms,
            new_attempt_id,
            String::new(),                      // attempt_policy_json
            None,                               // max_reclaim_count → 1000
            old_worker_instance_id,
            current_attempt_index,
        );
        match worker.claim_from_reclaim_grant(grant, args).await? {
            ReclaimExecutionOutcome::Claimed(handle) => {
                // 3. Retry the terminal write on the fresh lease.
                worker.complete(&handle, output).await
            }
            ReclaimExecutionOutcome::NotReclaimable { detail, .. } => {
                Err(structural_error(&format!(
                    "reclaim rejected: {detail}"
                )))
            }
            ReclaimExecutionOutcome::ReclaimCapExceeded {
                reclaim_count, ..
            } => {
                Err(structural_error(&format!(
                    "reclaim cap exceeded at {reclaim_count}"
                )))
            }
            ReclaimExecutionOutcome::GrantNotFound { .. } => {
                Err(structural_error("reclaim grant not found"))
            }
            _ => Err(structural_error("unknown reclaim outcome")),
        }
    }
    other => other,
}
```

The operator gap between `issue_reclaim_grant` and
`claim_from_reclaim_grant` is sub-100ms in-process; the
two-FCALL timing window is orders of magnitude tighter than #371's
original `complete`-vs-operator-pause gap (RFC-024 §7.3).

#### Handle-kind awareness

After `claim_from_reclaim_grant` returns `Claimed(handle)`, the
handle's `kind` is `HandleKind::Reclaimed` (distinct from
`HandleKind::Fresh` / `HandleKind::Resumed`). Downstream
metrics/tracing paths that already match on
`HandleKind::Fresh` vs `HandleKind::Resumed` should add a
`Reclaimed` arm to distinguish first-attempt vs resume-after-suspend
vs reclaim-after-lease-expiry observability. The enum is
`#[non_exhaustive]`; consumers can `_ =>` fallthrough instead.

#### `ReclaimGrant` vs `ResumeGrant` — do not confuse

Cairn migrators holding a pre-v0.12 `ReclaimGrant` variable: the
v0.12 `ReclaimGrant` is a **new, distinct** type for the
lease-reclaim path. The rename you need to apply to your existing
code is `ReclaimGrant → ResumeGrant` (the pre-RFC type was
always a resume grant — the name was the bug; RFC-024 §3.1). Only
reach for the new `ReclaimGrant` when wiring the #371 recovery
flow.

## What's next for cairn

- **Land the RFC-024 consumer migration** — replace the
  `F64 bridge retry loop` with the snippet above, drop the
  `remove once FF#371 ships` comment, close tracking issue #371.
- **Optional: capability-hash audit correlation** — the
  `IssueReclaimGrantRequest::capability_hash` field is an opaque
  token stored verbatim on the issued grant for audit /
  observability. It is **not** used for admission — admission
  compares `worker_capabilities` against the execution's
  `required_capabilities` (persisted on `exec_core` at
  `create_execution` time). Cairn can thread its existing
  admission-time capability-hash through for downstream
  correlation, or pass `None` to leave the field empty.

### 8. Agnostic-SDK PR-5.5 — `ClaimedTask` + `claim_from_grant` ungated

The SDK's worker hot paths (`claim_from_grant`, `claim_via_server`,
`claim_resumed_execution`, and the `ClaimedTask` type + its
non-streaming methods) are no longer `#[cfg(feature =
"valkey-default")]`-gated. They route through the `EngineBackend`
trait and compile under `--no-default-features, features = ["sqlite"]`.

`ClaimedExecution` and `ClaimedResumedExecution` gain a new
`handle: ff_core::backend::Handle` field populated by the owning
backend at claim time. The SDK's `ClaimedTask` caches the handle
and clones it into each per-op trait forwarder — replacing the
pre-PR `ValkeyBackend::encode_handle` synthesis call previously
hardcoded into `ClaimedTask::new`.

**Source-breaking — `#[non_exhaustive]` on claim contracts.**
`ClaimedExecution` and `ClaimedResumedExecution` are now marked
`#[non_exhaustive]` so future backend-populated fields stay
additive. Consumers that constructed these via struct literals,
or pattern-matched them without a `..` rest pattern, must switch
to the public constructors (`ClaimedExecution::new(..)`,
`ClaimedResumedExecution::new(..)`) or add `..` to exhaustive
destructures. Backends outside the crate cannot construct claim
results by literal — use the `::new` constructors that take the
`handle` as an explicit argument.

**Limitations.** Runtime coverage on PG/SQLite remains the
scheduler-routed [`claim_via_server`] path. Both backends return
[`EngineError::Unavailable`](../crates/ff-core/src/engine_error.rs)
from `EngineBackend::claim_execution` today, so `claim_from_grant`
on PG/SQLite is compile-reachable but runtime-unavailable. The
[`claim_via_server`] / [`Scheduler::claim_for_worker`] path is the
supported production shape for those backends in v0.12; a future
RFC-024 grant-consumer extension will wire the direct
`claim_from_grant` bodies. See
`project_claim_from_grant_pg_sqlite_gap.md` for status.

`ClaimedTask::read_stream` / `tail_stream` /
`tail_stream_with_visibility` remain `valkey-default`-gated in v0.12
— the backing `EngineBackend::read_stream` / `tail_stream` trait
methods are gated on ff-core's `streaming` feature, which the
`sqlite` feature set does not pull in today.

[`claim_via_server`]: https://docs.rs/ff-sdk/latest/ff_sdk/struct.FlowFabricWorker.html#method.claim_via_server
[`Scheduler::claim_for_worker`]: https://docs.rs/ff-scheduler

## Known limitations

RFC-023 + RFC-024 + the agnostic-SDK work in v0.12 expand the
backend-agnostic surface significantly, but a handful of paths remain
Valkey-first. None block the headline v0.12 consumer flows; all are
scoped for v0.13 RFC follow-up. Document these so consumers building
against PG / SQLite aren't surprised at runtime. The
`claim_from_grant` / `claim_via_server` runtime-`Unavailable` gap on
PG/SQLite is covered in §8 above; the items below round out the
surface.

### Scanner-bypass primitives — Valkey only

`EngineBackend::scan_eligible_executions`, `issue_claim_grant`, and
`block_route` (the agnostic-SDK PR-5 scanner primitives) ship as
Valkey-only in v0.12; PG and SQLite inherit the `Unavailable` trait
default. These methods exist to let benches / specialized tooling
bypass the scheduler; they are gated behind the bench-only
`direct-valkey-claim` feature on the SDK caller side and are **not**
a general consumer surface.

PG / SQLite deployments use the scheduler-routed
`FlowFabricWorker::claim()` path, which is fully supported and
handles eligibility, capability match, and admission server-side.
No consumer action needed.

### `read_current_attempt_index` asymmetry

All three backends implement `EngineBackend::read_current_attempt_index`
in v0.12, but the missing-row behaviour differs by design:

- **Valkey** — returns `AttemptIndex(0)` when `exec_core` is present
  but the `current_attempt_index` HGET field is absent or empty
  (matches pre-PR-3 inline SDK semantic). The downstream
  `claim_resumed_execution` FCALL surfaces the proper
  `NotAResumedExecution` / `ExecutionNotLeaseable` reject.
- **Postgres / SQLite** — column is `NOT NULL DEFAULT 0` so a
  pre-claim row naturally reads `0`, but a **missing** row surfaces
  as `EngineError::Validation { kind: InvalidInput }`.

The SDK only invokes this method on the resume-grant path after
`claim_from_resume_grant` admission has already resolved the
execution, so `exec_core` is guaranteed to exist and the asymmetry is
not consumer-observable under normal operation. Documented here so
downstream tooling that calls the trait method directly isn't
surprised. See rustdoc on
`EngineBackend::read_current_attempt_index` for the canonical
per-backend shape.

### v0.12 vs v0.11 bench baseline pending

Published v0.11 → v0.12 bench deltas (Valkey claim/complete p50/p99,
PG rev-7 cancel p99, SQLite baseline) are **not** captured in-repo at
tag time. GitHub Actions runners produce numbers too noisy to baseline
against (shared tenants, variable CPU steal, I/O contention), so they
are not used as the source of truth for release-to-release perf
comparisons.

Fresh baselines run on dedicated hosts post-tag will land in a
follow-up PR against the bench harness README. If you need pre-tag
perf signal for a PG / Valkey deployment decision, open an issue on
the repo — we can point at the most recent dedicated-host run.

## Non-changes

- No Rust API break on Valkey or Postgres hot paths **outside
  the RFC-024 rename-set above**.
- No wire-format change on the HTTP / JSON surface **other than
  the additive `POST /v1/executions/{id}/reclaim` endpoint**.
- Valkey backend claim/resume hot-path behaviour unchanged —
  reclaim is a new, additive path.

## Upgrade checklist

- [ ] `cargo update -p flowfabric` (or the ff-* sub-crates) to 0.12.0.
- [ ] **Postgres only:** run `sqlx migrate run` against every deployment
      before serving v0.12.0 traffic. Migration 0016 is forward-only.
- [ ] If you match exhaustively on `ServerError`, add a wildcard arm
      (or the new `SqliteRequiresDevMode` variant).
- [ ] If you construct `ServerConfig` via struct literal, switch to
      `ServerConfig::from_env()`, `ServerConfig::sqlite_dev()`, or
      `..Default::default()` spread.
- [ ] **Opting into SQLite dev harness (optional):** add
      `ff-backend-sqlite = "0.12"` (or `ff-sdk = { default-features =
      false, features = ["sqlite"] }`), set `FF_DEV_MODE=1` in your
      test harness, and follow the cairn-canonical pattern above.
      See [`docs/dev-harness.md`](dev-harness.md) for details.
- [ ] Run your integration smoke. File issues against this repo for
      any gap you hit; the RFC-023 design record is in
      `rfcs/RFC-023-sqlite-dev-only-backend.md`.
