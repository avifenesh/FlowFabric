# Consumer migration — FF #511 scheduler backend-agnostic (v0.15)

**Scope.** v0.15 closes FF #511 (cairn request): `ff_scheduler::Scheduler` no longer couples to `ferriskey::Client` as a hard requirement. Deployments that don't run Valkey (Postgres-only, SQLite dev) can now construct a scheduler — though they should generally continue to use their backend-native claim path (`PostgresScheduler`, `SqliteBackend::claim_for_worker`) rather than this scheduler, which stays Valkey-specialised for the scanner.

Read first: [FF #511](https://github.com/avifenesh/FlowFabric/issues/511).

## What changed

### Trait additions

Four new `EngineBackend` methods, all `core`-gated:

```rust
async fn release_admission(&self, args: ReleaseAdmissionArgs)
    -> Result<ReleaseAdmissionResult, EngineError>;

async fn read_quota_policy_limits(&self, quota_policy_id: &QuotaPolicyId)
    -> Result<Option<QuotaPolicyLimits>, EngineError>;

async fn block_execution_for_admission(&self, args: BlockExecutionForAdmissionArgs)
    -> Result<BlockExecutionForAdmissionOutcome, EngineError>;

async fn read_budget_usage_and_limits(&self, budget_id: &BudgetId)
    -> Result<BudgetUsageAndLimits, EngineError>;
```

Coverage:
- `release_admission` + `read_quota_policy_limits`: shipped on Valkey + Postgres + SQLite.
- `block_execution_for_admission` + `read_budget_usage_and_limits`: shipped on Valkey only. PG + SQLite stay at `Unavailable` — scheduler is Valkey-only territory (non-goal on PG per RFC-023); PG has `PostgresScheduler`, SQLite has `SqliteBackend::claim_for_worker`.

Four new `Supports.*` flags matching the trait methods.

### Scheduler struct shape

```rust
pub struct Scheduler {
    client: Option<ferriskey::Client>,
    backend: Weak<dyn EngineBackend>,
    config: PartitionConfig,
    // ...
}
```

- `backend` is `Weak` to break the Arc cycle with `ValkeyBackend.scheduler` (backend embeds the scheduler; a strong ref would leak both).
- `client` is optional: `Some` on Valkey deploys, `None` on backend-only ones.

### Constructor changes

Old signatures (breaking):
```rust
Scheduler::new(client: ferriskey::Client, config: PartitionConfig) -> Self
Scheduler::with_metrics(client, config, metrics) -> Self
```

New signatures (v0.15):
```rust
Scheduler::new(
    client: Option<ferriskey::Client>,
    backend: Weak<dyn EngineBackend>,
    config: PartitionConfig,
) -> Self

// New, backend-only:
Scheduler::new_with_backend(
    backend: Weak<dyn EngineBackend>,
    config: PartitionConfig,
) -> Self
```

## Migrating consumers

### Valkey consumer (cairn's current deploy)

```rust
// Before:
let sched = Scheduler::new(client.clone(), config);

// After:
let backend_arc: Arc<ValkeyBackend> = /* ... */;
let weak_trait: Weak<dyn EngineBackend> = Arc::downgrade(&backend_arc) as Weak<dyn EngineBackend>;
let sched = Scheduler::new(Some(client.clone()), weak_trait, config);
```

If constructing the backend + scheduler together (the common case), use `Arc::new_cyclic` so the scheduler's Weak points at the final Arc at construction time. `ValkeyBackend::install_scheduler_cyclic` helper ships for this — see `ff-server::Server::start_with_metrics` for the pattern.

### Postgres-only consumer (previously blocked by FF #511)

```rust
// Now possible:
let pg_backend: Arc<PostgresBackend> = /* ... */;
let weak_trait = Arc::downgrade(&pg_backend) as Weak<dyn EngineBackend>;
let sched = Scheduler::new_with_backend(weak_trait, config);

// `sched.claim_for_worker(...)` returns Ok(None) — the scanner path
// (ZRANGEBYSCORE / exec_core HGETs) has no trait primitive yet.
// PG consumers should use PostgresScheduler instead for real claims.
```

The FF #511 ask is satisfied: consumers can construct and call `Scheduler::claim_for_worker` without a `ferriskey::Client`, and it degrades to "no hit" rather than panicking. If you want actual claims on PG/SQLite, continue using the native claim paths (`PostgresScheduler`, `SqliteBackend::claim_for_worker`).

### Ports still pending

The scheduler retains raw Valkey calls for:
- ZRANGEBYSCORE on the per-partition eligible ZSET (scanner candidate read)
- HGET on `exec_core` for `blocking_reason`, `quota_policy_id`, `budget_ids` fields

These have no trait equivalents yet. A future RFC can lift them via a `read_exec_core_fields` extension or a typed scanner primitive, letting the scheduler fully drop the client. Not a v0.15 deliverable.

## Breaking changes

- `ff_scheduler::Scheduler::new` / `with_config` / `with_metrics` / `with_config_and_metrics` all require an `Arc::downgrade`-produced `Weak<dyn EngineBackend>` as the second argument. Wrap your existing `client.clone()` in `Some(...)`.
- `SchedulerError` gained an `EngineContext { source: Box<EngineError>, context: String }` variant. `valkey_kind()` returns `None` for this variant (the classified error wraps a typed `EngineError` with its own `.kind`). Existing callers that matched only `Valkey`/`ValkeyContext`/`Config` won't break (non-exhaustive match required).
- `BudgetChecker::check_budget` signature changed: `&ferriskey::Client` → `&dyn EngineBackend`. Callers in-workspace updated; out-of-workspace callers (unlikely given it's a specialized scheduler-internal API) would need to migrate.
- Non-UUID budget IDs no longer resolve to the pre-#511 `ff:budget:{b:0}:<raw>:limits` fallback — they now short-circuit to "no limits configured". Valkey + out-of-tree consumers should use `BudgetId::new()` (UUID) for all budgets.

## Why not fully drop the client in this release?

The scheduler's partition-scanner path (ZRANGEBYSCORE + exec_core HGET) has no trait primitive. Lifting it is a separate RFC — likely a `scan_eligible_executions`/`read_exec_core_fields`-shaped expansion. Doing it in the same release as the admission-primitive trait-route would have doubled the diff and the review surface. Shipping the admission primitives now (Phases 1/2a/2b/2c/3) unblocks cairn's PG trait-route ask; the scanner can follow.
