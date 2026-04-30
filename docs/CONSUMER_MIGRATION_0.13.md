# Consumer migration — v0.12 → v0.13

**Scope.** v0.13 is an ergonomics release on top of v0.12. This doc
covers the one consumer-visible SDK change in the release:
`FlowFabricAdminClient` is now a **backend-agnostic facade**,
constructible either over HTTP (existing behaviour) or directly over
a backend trait object (new). No breaking changes — existing
`connect` / `with_token` call-sites compile and behave identically.

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
- `WorkerConfig.backend: BackendConfig` dead-field under
  `connect_with` — unresolved from v0.12 `feedback_sdk_reclaim_ergonomics.md`
  Finding 2; separate follow-up.
