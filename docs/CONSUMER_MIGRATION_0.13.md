# Consumer migration — v0.12 → v0.13

**Scope.** v0.13 is an ergonomics release on top of v0.12. This doc
covers two consumer-visible SDK changes in the release:
1. `FlowFabricAdminClient` is now a **backend-agnostic facade**,
   constructible either over HTTP (existing behaviour) or directly
   over a backend trait object (new). No breaking changes — existing
   `connect` / `with_token` call-sites compile and behave identically.
2. `WorkerConfig.backend` is now `Option<BackendConfig>` so
   `FlowFabricWorker::connect_with` callers no longer need a
   placeholder `BackendConfig`. Breaking for struct-literal
   `connect` callers (wrap existing `BackendConfig` in `Some(...)`).

A per-change CHANGELOG listing is the source of truth; this doc
focuses on the ergonomic upgrade path.

## What changed

### `FlowFabricAdminClient` is now backend-agnostic (SC-10 follow-up)

**Motivation.** In v0.12 the SC-10 `incident-remediation` example
exposed a rough edge: `FlowFabricAdminClient` was HTTP-only and
required a running `ff-server`. Consumers running under
`FF_DEV_MODE=1` + SQLite — the zero-infra dev story RFC-023 sells —
could not use the SDK admin surface and had to drop down to
trait-direct `EngineBackend::issue_reclaim_grant`. The RFC-024
**worker** surface (`FlowFabricWorker::claim_from_reclaim_grant`)
was already backend-agnostic; the **admin** surface was not. v0.13
closes the parity gap.

**Shape.** `FlowFabricAdminClient` now internally holds a private
transport enum: `Http { reqwest::Client, base_url }` or
`Embedded(Arc<dyn EngineBackend>)`. The public method surface is
unchanged. Construction picks the transport:

```rust
// HTTP (existing — unchanged):
let admin = FlowFabricAdminClient::new("http://ff-server.dev")?;
let admin = FlowFabricAdminClient::with_token("https://ff-server", token)?;

// Embedded trait-dispatch (v0.13, new):
let backend: Arc<dyn EngineBackend> = SqliteBackend::new(uri).await?;
let admin = FlowFabricAdminClient::connect_with(backend);
```

The three admin methods —
`issue_reclaim_grant`, `claim_for_worker`, `rotate_waitpoint_secret` —
behave identically across transports.

## Migration

### If you currently use `FlowFabricAdminClient::new` or `with_token`

No action required. HTTP semantics, error shapes, and return values
are unchanged.

### If you currently trait-dispatch through `EngineBackend` to reach admin primitives

Switch to the facade for a cleaner consumer shape. Before/after from
`examples/incident-remediation`:

```rust
// Before (v0.12): supervisor calls the trait method directly.
async fn issue_grant(
    backend: &dyn EngineBackend,
    exec_id: &ExecutionId,
    lane: &LaneId,
    worker_id: &str,
    worker_instance_id: &str,
) -> Result<ReclaimGrant> {
    let outcome = backend
        .issue_reclaim_grant(IssueReclaimGrantArgs::new(
            exec_id.clone(),
            WorkerId::new(worker_id),
            WorkerInstanceId::new(worker_instance_id),
            lane.clone(),
            None,
            60_000,
            None,
            None,
            Default::default(),
            TimestampMs::from_millis(now_ms()),
        ))
        .await?;
    match outcome {
        IssueReclaimGrantOutcome::Granted(g) => Ok(g),
        other => anyhow::bail!("expected Granted, got {other:?}"),
    }
}

// After (v0.13): supervisor drives the agnostic admin client.
async fn issue_grant(
    admin: &FlowFabricAdminClient,
    exec_id: &ExecutionId,
    lane: &LaneId,
    worker_id: &str,
    worker_instance_id: &str,
) -> Result<ReclaimGrant> {
    let resp = admin
        .issue_reclaim_grant(
            exec_id.as_str(),
            IssueReclaimGrantRequest {
                worker_id: worker_id.into(),
                worker_instance_id: worker_instance_id.into(),
                lane_id: lane.as_str().to_owned(),
                capability_hash: None,
                grant_ttl_ms: 60_000,
                route_snapshot_json: None,
                admission_summary: None,
                worker_capabilities: Vec::new(),
            },
        )
        .await?;
    match resp {
        IssueReclaimGrantResponse::Granted { .. } => resp.into_grant(),
        IssueReclaimGrantResponse::NotReclaimable { detail, .. } => {
            anyhow::bail!("expected Granted, got NotReclaimable: {detail}")
        }
        IssueReclaimGrantResponse::ReclaimCapExceeded { reclaim_count, .. } => {
            anyhow::bail!("cap hit: {reclaim_count}")
        }
    }
}
```

Construction site:

```rust
// v0.13: one line replaces the pattern of "hold trait_obj, rebuild
// IssueReclaimGrantArgs at every call-site".
let admin = FlowFabricAdminClient::connect_with(trait_obj.clone());
```

## Error-surface notes

- The embedded transport translates `EngineError::Unavailable`
  (emitted by backends that have not implemented a given method) into
  `SdkError::AdminApi { status: 503, kind: Some("unavailable"), ... }`
  so callers see a uniform admin-error surface regardless of
  transport.
- Other `EngineError` variants surface as `SdkError::Engine(..)`
  unchanged.
- Request-body validation **rules** mirror `ff-server`'s handler so
  the embedded transport rejects the same inputs the HTTP transport
  does. The exact `SdkError` variant differs — embedded-path
  rejections surface as `SdkError::Config` (no HTTP round-trip) while
  HTTP surfaces `SdkError::AdminApi` with status `400`. Match on
  `SdkError::is_retryable` or on `Config | AdminApi` together if you
  need transport-independence.

## Divergence from the HTTP transport

The embedded transport has no ff-server config surface to read from,
so two behaviours are pinned to documented defaults rather than
per-deployment values:

- `rotate_waitpoint_secret` forwards `EMBEDDED_WAITPOINT_HMAC_GRACE_MS`
  (24 h, matching `ff-server`'s default `FF_WAITPOINT_HMAC_GRACE_MS`)
  as the per-partition grace window. Operators who need a non-default
  grace should use the HTTP transport against an `ff-server` with
  `FF_WAITPOINT_HMAC_GRACE_MS` set.
- No single-writer admin semaphore, no audit-log emission. These are
  `ff-server` responsibilities; embedded consumers wanting them bring
  their own gate.

## PR-7b Cluster 4 — completion listener is now trait-routed

**What changed.** The post-completion cascade path
(`ff-engine::completion_listener::spawn_dispatch_loop`) previously
held backend-specific logic — a `ferriskey::Client`-driven FCALL walk
for Valkey and a separate `run_completion_listener_postgres` draining
the PG `ff_completion_event` outbox. v0.13 unifies both behind
`EngineBackend::cascade_completion(&CompletionPayload)`:

- `spawn_dispatch_loop(backend, stream, shutdown)` — now takes
  `Arc<dyn EngineBackend>` instead of `(router, client)`.
- New trait method `EngineBackend::cascade_completion` with a default
  impl returning `EngineError::Unavailable` for out-of-tree backends
  that have not migrated.

**Timing semantics — documented divergence, NOT a parity gap.** The
two in-tree backends cascade with different timing guarantees. This
is architectural and intentional:

- **Valkey — synchronous.** By the time `cascade_completion` returns,
  the full recursive cascade (up to `MAX_CASCADE_DEPTH = 50`) has run
  inline. `CascadeOutcome.synchronous = true`;
  `resolved + cascaded_children` reflect the whole subtree walked.
- **Postgres — async via outbox.** The call resolves the payload to
  its `ff_completion_event.event_id` and invokes Wave-5a
  `dispatch_completion` (per-hop serializable transactions; see
  `ff_backend_postgres::dispatch`). Further-descendant cascades ride
  their own outbox events emitted by those per-hop transactions —
  NOT this call. `CascadeOutcome.synchronous = false`.

Papering over the divergence would be wrong: making PG synchronously
wait for outbox drain would kill PG throughput; making Valkey publish
to an outbox first would double-work on Valkey and change its
semantics. Both backends achieve cascade — just with different timing.

**Consumer action.** If your code depends on synchronous cascade
observable from the completion call, one of:

1. Target Valkey explicitly and assert
   `CascadeOutcome.synchronous == true`.
2. On Postgres, observe outbox drain via the
   `dependency_reconciler` partial index
   (`ff_completion_event.dispatched_at_ms IS NOT NULL`) before
   asserting graph state.

If you only call the engine's `start_with_completions` and never
invoke `cascade_completion` directly, **no action required** — the
engine wiring now passes the backend through; the behaviour you saw
in v0.12 (Valkey: sync-through-FCALL; PG: drain-through-outbox) is
preserved exactly.

**Back-compat shims retained.** `run_completion_listener_postgres`
still exists for in-tree test callers, now taking
`(Arc<dyn EngineBackend>, Arc<dyn CompletionBackend>, PgPool, ShutdownRx)`.
The extra `engine_backend` argument is mechanical (pass the same
`Arc<PostgresBackend>` as both).

## Not in scope

- HTTP admin semantics (`ff-server` unchanged).
- New admin methods.
- `rotate_waitpoint_hmac_secret_all_partitions` free fn stays
  Valkey-gated (Valkey-specific FCALL fan-out; use the facade's
  `rotate_waitpoint_secret` for the agnostic path).
(Closed in this release: `WorkerConfig.backend: BackendConfig` dead
under `connect_with` — see the dedicated section below.)

## What also changed

### `WorkerConfig.backend` is now `Option<BackendConfig>` (SC-10 follow-up)

**Motivation.** The v0.12 SC-10 `incident-remediation` example
surfaced a second rough edge on top of the admin-HTTP-only one:
`WorkerConfig.backend` was eagerly required, but only consumed by
`FlowFabricWorker::connect` (URL-based Valkey dial). The
`connect_with` path — backend-agnostic, takes a pre-built
`Arc<dyn EngineBackend>` — ignored the field entirely, forcing
every `connect_with` caller (including the SC-10 SQLite supervisor)
to carry a placeholder `BackendConfig::valkey("127.0.0.1", 6379)`
just to satisfy the struct literal. Cairn flagged this as Finding 2
in `feedback_sdk_reclaim_ergonomics.md`; v0.13 closes it.

**Shape.** The field type changes from `BackendConfig` to
`Option<BackendConfig>`. Semantics:

- `Some(cfg)` + `FlowFabricWorker::connect` — unchanged. `cfg`
  drives the dial.
- `None` + `FlowFabricWorker::connect` — rejected with
  `SdkError::Config { context: "worker_config", field:
  Some("backend"), .. }`. The URL-based path needs a
  `BackendConfig` to dial; silently defaulting would hide caller
  mistakes.
- `None` + `FlowFabricWorker::connect_with` — the clean path.
  The injected backend is authoritative.
- `Some(cfg)` + `FlowFabricWorker::connect_with` — accepted, but
  logs a `tracing::warn!` noting that `cfg` is ignored. Useful
  for callers mid-migration; set the field to `None` to silence.

**Before (v0.12, `connect_with` path):**

```rust,ignore
let config = WorkerConfig {
    backend: BackendConfig::valkey("127.0.0.1", 6379), // placeholder — ignored
    worker_id: WorkerId::new("w1"),
    // …
};
FlowFabricWorker::connect_with(config, backend, None).await?;
```

**After (v0.13, `connect_with` path):**

```rust,ignore
let config = WorkerConfig {
    backend: None, // explicit: this path takes the injected backend
    worker_id: WorkerId::new("w1"),
    // …
};
FlowFabricWorker::connect_with(config, backend, None).await?;
```

**After (v0.13, `connect` path — minimal change):**

```rust,ignore
let config = WorkerConfig {
    backend: Some(BackendConfig::valkey("localhost", 6379)), // wrap in Some
    worker_id: WorkerId::new("w1"),
    // …
};
FlowFabricWorker::connect(config).await?;
```

## Tag methods — `set_execution_tag` / `set_flow_tag` / `get_execution_tag` / `get_flow_tag` (#439 / cairn #433)

**Motivation.** Pre-v0.13 consumers setting caller-namespaced tags
(e.g. `cairn.session_id`) had to drop down to raw `ferriskey::Client`
`HSET` calls against internal partition layouts. That bypassed
FlowFabric's namespace validation, coupled cairn to the Valkey wire
shape, and was unreachable from PG/SQLite consumers. v0.13 adds four
trait-routed tag methods with identical shape across all three
backends.

**Namespace rule.** Tag keys MUST match the regex
`^[a-z][a-z0-9_]*\.[a-z0-9_][a-z0-9_.]*$` — i.e. `<caller>.<field>`
with a literal dot separator. The key carries its own namespace
(`cairn.session_id`, not `session_id` + a separate namespace arg).
Values are arbitrary UTF-8. The trait-side
`ff_core::engine_backend::validate_tag_key` is the parity-of-record
and runs on every backend before the wire hop; Valkey's Lua path
additionally validates (more permissively, prefix-only) on the
storage tier.

**Before (v0.12, cairn's pattern):**

```rust,ignore
// Raw Valkey HSET bypassing FlowFabric's namespace rules.
let mut client = ferriskey::Client::connect(valkey_url).await?;
client
    .hset(
        format!("ff:exec:{{p:{}}}:{}:tags", partition, exec_id),
        "cairn.session_id",
        session_id.as_str(),
    )
    .await?;
```

**After (v0.13, trait-routed on any backend):**

```rust,ignore
let backend: Arc<dyn EngineBackend> = pg_backend.clone();
backend
    .set_execution_tag(&exec_id, "cairn.session_id", session_id.as_str())
    .await?;

// Read-side:
let v = backend
    .get_execution_tag(&exec_id, "cairn.session_id")
    .await?;

// Flow-scoped tags land on a separate store (see engine_backend rustdoc
// for the per-backend shape — Valkey uses a dedicated :tags sub-hash;
// PG/SQLite write the top-level raw_fields on ff_flow_core):
backend
    .set_flow_tag(&flow_id, "cairn.tenant", tenant.as_str())
    .await?;
```

**Error mapping.**

- Invalid key → `EngineError::Validation { kind: InvalidInput, detail }`
  (short-circuits before the wire hop).
- Missing execution → `EngineError::NotFound { entity: "execution" }`
  (matches the Valkey FCALL `execution_not_found` mapping).
- Missing flow → `EngineError::NotFound { entity: "flow" }`.
- On read, missing tag **and** missing row both collapse to
  `Ok(None)` — callers that need to distinguish must call
  `describe_execution` / `describe_flow` first.

**Describe-flow parity caveat.** `FlowSnapshot::tags` from
Valkey's `describe_flow` snapshots `flow_core` fields only and does
NOT today merge the `:tags` sub-hash; Postgres `describe_flow` surfaces
flow tags via `extract_tags` on `raw_fields`. Consumers on Valkey that
need the full flow-tag set should complement the snapshot with per-key
`get_flow_tag` reads until the Valkey merge lands (additive
post-v0.13).

## Waitpoint-token read — `read_waitpoint_token` (#438 / cairn #434)

**Motivation.** The signal-bridge (cairn's resume-on-signal harness)
needs to read the stored waitpoint token at poll-on-resume to
reconstruct the HMAC fingerprint. Pre-v0.13 the only path was
Valkey-only `fetch_waitpoint_token_v07` (retired at v0.8.0) or raw
`HGET` against an internal partition layout. v0.13 adds a
backend-agnostic trait method — all three backends implement it.

**Before (v0.12, Valkey-only):**

```rust,ignore
// Direct Valkey HGET bypassing the trait.
let mut client = ferriskey::Client::connect(valkey_url).await?;
let token: Option<String> = client
    .hget(
        format!("ff:wp:{{p:{}}}:{}", partition, waitpoint_id),
        "token",
    )
    .await?;
```

**After (v0.13):**

```rust,ignore
let token = backend
    .read_waitpoint_token(partition, &waitpoint_id)
    .await?;
```

`partition` is the opaque `PartitionKey` the waitpoint was minted
against — typically extracted from the `Handle` / `ResumeToken` the
signal-bridge holds. Missing waitpoint surfaces as `Ok(None)` on all
three backends.

## Service-layer FCALL surface (#442 / cairn #389)

**Motivation.** Control-plane callers (cairn's
`valkey_control_plane_impl.rs` and future non-Handle consumers) were
reaching into `ferriskey::Value::Array` matching + a manual
`check_fcall_success` + `parse_*` for the full `complete` / `fail` /
`renew` / `resume` / admission / eligibility surface. v0.13 adds six
trait methods that mirror the existing Handle-taking peers but accept
`(execution_id, fence)` tuples and return typed outcomes from
`ff_core::contracts`.

**Parity status at v0.13.** Valkey ships bodies at landing. PG +
SQLite inherit the `EngineError::Unavailable { op: "<name>" }` default
and receive real bodies in a follow-up (same staging as RFC-024
reclaim + `claim_execution`). See
[`POSTGRES_PARITY_MATRIX.md`](POSTGRES_PARITY_MATRIX.md) §cairn #389
for the per-method row.

**Before (v0.12):**

```rust,ignore
// Raw ferriskey Value matching against the FCALL response.
let resp = client
    .fcall(
        "ff_complete_execution",
        &keys_for_exec(&partition, &exec_id),
        &args_for_complete(&fence, &result_payload),
    )
    .await?;
let arr = match resp {
    ferriskey::Value::Array(a) => a,
    other => anyhow::bail!("unexpected FCALL shape: {other:?}"),
};
check_fcall_success(&arr)?;
let outcome = parse_complete_execution_response(&arr)?;
```

**After (v0.13):**

```rust,ignore
let outcome = backend
    .complete_execution(&exec_id, &fence, &result_payload)
    .await?;

// Sibling peers, same pattern:
backend.fail_execution(&exec_id, &fence, &failure).await?;
backend.renew_lease(&exec_id, &fence).await?;
backend.resume_execution(&exec_id, &fence, &resume_args).await?;

// Admission / eligibility — read-shaped:
let admission = backend
    .check_admission(&quota_policy_id, "default", &args)
    .await?;
let status = backend.evaluate_flow_eligibility(&flow_id).await?;
```

`check_admission` takes `quota_policy_id` + `dimension` outside the
`CheckAdmissionArgs` struct because quota keys live on the
`{q:<policy>}` partition (not derivable from `execution_id`); empty
`dimension` is normalised to `"default"`.

## cairn #454 trait additions — 4 new control-plane methods

**Motivation.** cairn #454 asks for four additional control-plane
methods that the current trait does not expose: per-execution
**budget spend** with tenant-open dimensions, per-execution
**budget-attribution release**, **operator approval-signal** delivery
(token never leaves the server), and **backend-atomic
`issue_claim_grant + claim_execution`** composition. v0.13 lands the
trait surface + arg/result types + `Unavailable` defaults; per-backend
bodies land in the subsequent phases of the same release window
(Valkey → PG → SQLite).

**New trait methods.** All four live on `EngineBackend` under
`#[cfg(feature = "core")]` and return `EngineError::Unavailable`
from the default impl so out-of-tree backends compile unchanged.

| Method | Args | Return |
|---|---|---|
| `record_spend` | [`RecordSpendArgs`] | [`ReportUsageResult`] |
| `release_budget` | [`ReleaseBudgetArgs`] | `()` |
| `deliver_approval_signal` | [`DeliverApprovalSignalArgs`] | [`DeliverSignalResult`] |
| `issue_grant_and_claim` | [`IssueGrantAndClaimArgs`] | [`ClaimGrantOutcome`] |

**Shape notes (why these types, not the existing peers).**

- `RecordSpendArgs.deltas` is `HashMap<String, u64>` — **open-set**
  tenant-defined dimension keys, distinct from the fixed-shape
  `UsageDimensions` used by `report_usage` / `report_usage_admin`.
  Cairn budgets are per-tenant open-schema (tenant A tracks
  `"tokens"` + `"cost_cents"`; tenant B tracks `"egress_bytes"`).
  Return reuses `ReportUsageResult` — same four variants cairn's UI
  already branches on. No new result enum.
- `ReleaseBudgetArgs` is **per-execution**, not whole-budget flush.
  Called on execution termination to reverse this execution's
  attribution; the budget counter persists across executions.
- `DeliverApprovalSignalArgs` is a pre-shaped variant of
  `deliver_signal` where the caller **does not carry the waitpoint
  token**. The backend reads the token from `ff_waitpoint_pending`
  (via `read_waitpoint_token`, v0.12-shipped), HMAC-verifies
  server-side, and dispatches. Operator API never touches the bytes.
  `signal_name` is a flat string (`"approved"` / `"rejected"` by
  convention); audit metadata (`decided_by`, `note`, `reason`) lives
  in cairn's audit log, not on the FF surface.
- `IssueGrantAndClaimArgs` + `ClaimGrantOutcome` compose
  `issue_claim_grant` + `claim_execution` into a **backend-atomic**
  op. Caller-chained composition risks leaking a grant when
  `claim_execution` fails after `issue_claim_grant` succeeded; the
  trait method's contract is that Valkey fuses them in one FCALL and
  PG/SQLite fuse them inside one tx. The default impl is **not** a
  fallback chained call — it returns `Unavailable` so consumers
  cannot silently pick up a non-atomic path.

**Parity status at v0.13.** Trait + types + Unavailable defaults
land in this release. Valkey bodies land next (ship vehicle: #454
Phase 3). PG + SQLite bodies follow (#454 Phases 4 + 5). Consumers
compile-check against `Unavailable` on PG / SQLite until the
per-backend phases merge.

**Before (v0.12, cairn's pattern for record_spend):**

```rust,ignore
// cairn had no trait seat; budget spend went through ad-hoc ff-server
// HTTP endpoints + cairn-local bookkeeping.
```

**After (v0.13):**

```rust,ignore
let deltas = std::collections::HashMap::from([
    ("tokens".into(), 1_500),
    ("cost_cents".into(), 12),
]);
let outcome = backend
    .record_spend(RecordSpendArgs::new(
        budget_id,
        exec_id,
        deltas,
        idempotency_key,
    ))
    .await?;
match outcome {
    ReportUsageResult::Ok { .. } => { /* proceed */ }
    ReportUsageResult::SoftBreach { .. } => { /* warn + proceed */ }
    ReportUsageResult::HardBreach { .. } => { /* stop */ }
    ReportUsageResult::AlreadyApplied { .. } => { /* dedup no-op */ }
}
```

## PR-7b scanner trait routing (#436 / cluster PRs)

**Motivation.** The scanner supervisor (`ff-engine`) previously held
Valkey-specific FCALL dispatch logic for every scanner tick —
foundation (`mark_lease_expired_if_due`, `promote_delayed`,
`close_waitpoint`, `expire_execution`, `expire_suspension`),
reconciler (`unblock_execution`), cancel-family
(`drain_sibling_cancel_group`, `reconcile_sibling_cancel_group`),
tally-recompute (`reconcile_{execution_index,budget_counters,quota_counters}`),
projection (`project_flow_summary`), and retention (`trim_retention`).
Consumers embedding `Engine` on Postgres or SQLite saw `Unsupported`
from every tick. v0.13 routes every scanner tick through
`EngineBackend::*` with backend-appropriate semantics.

**Cluster index.**

| Cluster | PR | Trait methods |
|---|---|---|
| 1 — Foundation | #441 + #443 | `mark_lease_expired_if_due`, `promote_delayed`, `close_waitpoint`, `expire_execution`, `expire_suspension`, `get_execution_namespace` |
| 2 — Reconciler | #445 | `unblock_execution` |
| 3 — Cancel family | #444 | `drain_sibling_cancel_group`, `reconcile_sibling_cancel_group` |
| 2b-A — Tally-recompute | #447 | `reconcile_execution_index`, `reconcile_budget_counters`, `reconcile_quota_counters` |
| 2b-B — Projection + retention | #449 (pending merge) | `project_flow_summary`, `trim_retention` |
| 4 — Completion listener | #446 | `cascade_completion` (covered in the Cluster 4 section above) |

**Consumer-visible shape.** Consumers embedding `Engine` get
backend-agnostic scanner routing post-PR-7b final scaffolding:

```rust,ignore
// Post-PR-7b: Engine::start_with_completions is backend-agnostic.
let pg_backend: Arc<PostgresBackend> = PostgresBackend::connect_with(...).await?;
let engine = Engine::start_with_completions(
    config,
    pg_backend.clone().into_arc_dyn(),  // backend as Arc<dyn EngineBackend>
    completion_stream,
    shutdown,
)
.await?;
```

On Postgres + SQLite, the reconcile/cancel/tally scanner ticks that
map to Valkey-only FCALL semantics surface `Unsupported` and the
scanner supervisor skips them — **intentional**, because the PG
scheduler and SQLite's `BEGIN IMMEDIATE` reconcilers re-evaluate the
same state live via SQL. See
[`POSTGRES_PARITY_MATRIX.md`](POSTGRES_PARITY_MATRIX.md) §PR-7b
scanner additions for the per-method rationale.

## cairn closed-issues index

v0.13 closes the following cairn-originated ask items. Each row
points to the tracking FlowFabric issue/PR pair.

| cairn issue | FlowFabric resolution | Ship vehicle |
|---|---|---|
| #433 Tag methods on trait | #439 | v0.13 (this release) |
| #434 Waitpoint-token read on trait | #438 | v0.13 |
| #435 `pg.create_waitpoint` | #437 | v0.13 |
| #387 `ferriskey` ReadOnly auto-refresh | #426 + #427 + #429 chain | v0.12 (realised) |
| #389 Typed FCALL outcomes (service-layer) | #442 | v0.13 (Valkey); PG + SQLite bodies follow-up |
| #436 PR-7b — `Engine` non-Valkey backend routing | #441 + #443 + #444 + #445 + #446 + #447 + #449 + final scaffolding | v0.13 |

**Per-issue summary.**

- **#433 (→ #439).** Caller-namespaced tag writes + reads now route
  through `EngineBackend::{set,get}_{execution,flow}_tag` on all
  three backends. Regex-validated at the trait edge. See §Tag
  methods above.
- **#434 (→ #438).** `read_waitpoint_token` implemented on all three
  backends; replaces the retired `fetch_waitpoint_token_v07`. See
  §Waitpoint-token read above.
- **#435 (→ #437).** Postgres `create_waitpoint` landed as a trait
  impl, closing the ingress-parity gap flagged during cairn's
  signal-bridge port.
- **#387 (→ #426 + #427 + #429 chain).** `ferriskey` ReadOnly clients
  now auto-refresh on topology changes without caller intervention.
  Realised in v0.12; kept in this index for completeness.
- **#389 (→ #442).** Six service-layer trait methods added
  (`complete_execution`, `fail_execution`, `renew_lease`,
  `resume_execution`, `check_admission`,
  `evaluate_flow_eligibility`). See §Service-layer FCALL surface
  above. PG + SQLite parity is v0.13-follow-up scope.
- **#436 (→ 7+ PRs).** PR-7b trait-routed scanner push lands every
  cluster 1/2/3/2b-A/2b-B/4 scanner tick behind the `EngineBackend`
  trait. Cluster 2b-B (projection + retention) depends on #449
  merging.

## Known limitations

The following Postgres / SQLite `Unavailable` surfaces persist in
v0.13 and are **not** closed by this release. All are tracked;
consumers on PG or SQLite should compile-check against the matrix
before relying on these methods.

- **`claim_from_grant` / `claim_execution` / `claim_via_server`
  still PG/SQLite `Unavailable`** per
  `project_claim_from_grant_pg_sqlite_gap.md`. v0.13 does NOT
  address this. PG's grant-consumer flow routes through
  `PostgresScheduler::claim_for_worker`; SQLite has no
  grant-consumer path today.
- **`resolve_dependency` PG `Unavailable`** — PG uses an
  outbox-per-event cascade (`ff_completion_event` +
  `dispatch_completion`), not the Valkey per-edge shape. Not a bug;
  an architectural divergence — the PG reconciler already drives
  the equivalent work.
- **`unblock_execution` PG `Unavailable`** — PG re-evaluates
  eligibility live via SQL on every `claim_for_worker` tick; no
  persisted blocked-index to reconcile. SQLite inherits the design.
- **Scanner-bypass primitives Valkey-only** —
  `scan_eligible_executions`, `issue_claim_grant`, `block_route`
  remain Valkey-`impl` / PG + SQLite `Unavailable` (default). **Not
  a regression** — design intent; gated behind the
  `direct-valkey-claim` bench-only feature. PG/SQLite consumers use
  the scheduler-routed `claim_for_worker` path.
- **Retention overrides Valkey-only post-#449** —
  `policy.stream_policy.retention_ttl_ms` per-policy overrides are
  honoured only on Valkey's `trim_retention` FCALL. Postgres uses
  the global default. Not a regression; documented when #449 lands.
- **Scanner `Unavailable` on PG/SQLite by design** — the six
  reconcile/cancel/tally scanner methods
  (`drain_sibling_cancel_group`, `reconcile_sibling_cancel_group`,
  `unblock_execution`, `reconcile_execution_index`,
  `reconcile_budget_counters`, `reconcile_quota_counters`) return
  `Unavailable` on PG/SQLite — **not a parity gap**, because these
  backends' SERIALIZABLE transactions + live SQL eligibility
  re-evaluation leave no counter drift to reconcile. See the matrix
  for per-method rationale.

## Deferred to v0.14

The following items were scoped out of v0.13 and are on the v0.14
track. Trackers listed inline.

- **Broader `Duration` overflow harmonisation** — scattered
  `u64::MAX` ms-to-`Duration` conversions need a project-wide
  policy pass. Tracked on GitHub issues.
- **Option B streaming trait split** — the streaming-feature-gated
  subtrait split was considered but deferred; current
  `#[cfg(feature = "streaming")]` gates on individual trait methods
  ship v0.13 unchanged.
- **PG / SQLite `claim_execution` parity** — grant-consumer flow
  for the SDK `claim_from_grant` path. See
  `project_claim_from_grant_pg_sqlite_gap.md`.
- **SQLite `public_state = 'running'` on claim path** — see
  `project_sqlite_claim_public_state_gap.md`. Normaliser handles
  the read side; claim-write parity is a v0.14 fix.
- **Remaining PG `Unavailable` surfaces on the service-layer family**
  — bodies for `complete_execution`, `fail_execution`,
  `renew_lease`, `resume_execution`, `check_admission`,
  `evaluate_flow_eligibility` on PG + SQLite. Covered by #442
  follow-up.
- **Cluster 2b-B rows in the parity matrix** —
  `project_flow_summary` and `trim_retention` rows merge
  concurrently with PR #449; if #449 slips past the v0.13 tag,
  the rows land in a point-release doc amendment.
