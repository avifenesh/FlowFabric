# Consumer migration — RFC-025 worker registry (v0.14)

**Scope.** v0.14 lands four + one new `EngineBackend` trait methods
covering worker-pool lifecycle + lease-expiry enumeration + live-worker
listing (RFC-025). This doc is the migration guide for consumers —
primarily cairn — whose in-tree code previously drove these primitives
via bespoke per-backend commands (Valkey HSET/ZRANGEBYSCORE etc).

Read first: [`rfcs/RFC-025-worker-registry.md`](../rfcs/RFC-025-worker-registry.md).

## What changed

### Trait surface (new methods on `EngineBackend`)

```rust
// Feature gate: `core`.
async fn register_worker(&self, args: RegisterWorkerArgs)
    -> Result<RegisterWorkerOutcome, EngineError>;

async fn heartbeat_worker(&self, args: HeartbeatWorkerArgs)
    -> Result<HeartbeatWorkerOutcome, EngineError>;

async fn mark_worker_dead(&self, args: MarkWorkerDeadArgs)
    -> Result<MarkWorkerDeadOutcome, EngineError>;

async fn list_workers(&self, args: ListWorkersArgs)
    -> Result<ListWorkersResult, EngineError>;

// Feature gate: `all(core, suspension)`.
async fn list_expired_leases(&self, args: ListExpiredLeasesArgs)
    -> Result<ListExpiredLeasesResult, EngineError>;
```

Every in-tree backend (Valkey, Postgres, SQLite) ships bodies for all
five. `Supports::{register_worker, heartbeat_worker, mark_worker_dead,
list_expired_leases, list_workers}` all return `true` on every in-tree
backend.

### Key decisions (RFC §9 locked)

1. **Namespace-prefixed Valkey keys:** `ff:worker:{ns}:{inst}:alive`,
   `ff:worker:{ns}:{inst}:caps`, `ff:idx:{ns}:workers`. Cross-tenant
   instance_id collisions are impossible at the key layer.
2. **`ff_register_worker` is idempotent:** re-registering with the
   same `worker_instance_id` overwrites caps + lanes + TTL (operational
   ergonomics; hot-reloading a worker with updated caps Just Works).
   Re-registering with a DIFFERENT `worker_id` under the same instance
   is rejected with `Validation(InvalidInput, "instance_id reassigned")`.
3. **`list_expired_leases` cursor is tuple** `(expires_at_ms, ExecutionId)`
   so pagination is stable under equal-expiry.
4. **`list_workers` is namespace-scoped** — cross-namespace is rejected
   with `Unavailable`. Operator tooling loops per-namespace.

### Backend-specific notes

| | Valkey | Postgres | SQLite |
|-|-|-|-|
| Register | New atomic `ff_register_worker` FCALL (v32) | `INSERT … ON CONFLICT DO UPDATE RETURNING (xmax=0)` | Preflight SELECT + `INSERT … ON CONFLICT DO UPDATE` in tx |
| TTL | Native PEXPIRE | 30s `ttl_sweep` scanner | 30s `ttl_sweep` scanner |
| Storage | `ff:worker:{ns}:{inst}:{alive,caps}` hashes | `ff_worker_registry` (256-way HASH) + `ff_worker_registry_event` | Flat `ff_worker_registry` + event (no partitioning per RFC-023 §4.1 A3) |
| `lanes` | CSV in caps hash | `text[]` | sorted-joined CSV |

## Migrating cairn (or any consumer with bespoke per-backend impls)

### Before (cairn `valkey_impl.rs`, pre-v0.14)

```rust
impl Engine for CairnEngine {
    async fn register_worker(&self, args: RegisterWorkerArgs) -> Result<(), Error> {
        self.client.cmd("HSET").arg("ff:worker:...").arg("capabilities").arg(csv).execute().await?;
        self.client.cmd("SADD").arg("ff:workers:active").arg(&args.worker_id).execute().await?;
        Ok(())
    }
    async fn heartbeat_worker(&self, w: &WorkerId, now: i64) -> Result<(), Error> { ... }
    async fn mark_worker_dead(&self, w: &WorkerId, reason: &str) -> Result<(), Error> { ... }
    async fn list_expired_leases(&self, now_ms: i64) -> Result<Vec<LeaseInfo>, Error> { ... }
}
```

And a PG sibling that returned `EngineError::Unavailable` via the
`cairn #PR-C4` gate.

### After (cairn `engine.rs`, post-v0.14)

```rust
use ff_core::contracts::{
    RegisterWorkerArgs, HeartbeatWorkerArgs, MarkWorkerDeadArgs,
    ListExpiredLeasesArgs, ListWorkersArgs,
};
use ff_core::engine_backend::EngineBackend;
use std::sync::Arc;

pub struct CairnEngine {
    backend: Arc<dyn EngineBackend>,
}

impl CairnEngine {
    pub async fn register_worker(
        &self,
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        lanes: BTreeSet<LaneId>,
        capabilities: BTreeSet<String>,
        liveness_ttl_ms: u64,
        namespace: Namespace,
        now: TimestampMs,
    ) -> Result<(), CairnError> {
        let args = RegisterWorkerArgs::new(
            worker_id, worker_instance_id, lanes,
            capabilities, liveness_ttl_ms, namespace, now,
        );
        match self.backend.register_worker(args).await? {
            RegisterWorkerOutcome::Registered | RegisterWorkerOutcome::Refreshed => Ok(()),
        }
    }

    pub async fn heartbeat_worker(
        &self,
        worker_instance_id: WorkerInstanceId,
        namespace: Namespace,
        now: TimestampMs,
    ) -> Result<HeartbeatWorkerOutcome, CairnError> {
        let args = HeartbeatWorkerArgs::new(worker_instance_id, namespace, now);
        Ok(self.backend.heartbeat_worker(args).await?)
    }

    // … similar for mark_worker_dead, list_expired_leases, list_workers
}
```

**Drop** the bespoke Valkey HSET/ZRANGEBYSCORE paths and the PG
`Unavailable` stub. Both backends now serve the trait method
authoritatively.

### Naming map (cairn upstream → FF)

| cairn upstream | FF |
|-|-|
| `worker_id` | `worker_instance_id` (process identity, per-boot) |
| *(no equivalent)* | `worker_id` (pool identity, stable across restarts) |
| `now_ms` | `TimestampMs::from_millis(…)` |
| `reason: &str` | `reason: String` (≤256 B, no control chars) |

See RFC-025 §7 glossary. Cairn's adapter layer is the right place to
perform the rename — FF's trait doesn't bend to consumer naming.

## Testing your migration

1. **Unit-level:** swap `valkey_impl::register_worker` with
   `backend.register_worker(...)` in an existing test; assert the same
   caps + TTL + index membership. Cross-crate compat fixtures for
   this are in `crates/ff-test/tests/worker_registry_*.rs`
   (Phase 6).
2. **Integration-level:** point cairn at a v0.14 `ff-server`; run
   cairn's worker-lifecycle e2e suite. The one behaviour difference
   from pre-v0.14 is the Valkey key shape — see §9.1 of the RFC.
3. **Production cutover:** Valkey keys change shape from `ff:worker:{id}:alive`
   to `ff:worker:{ns}:{id}:alive`. Operators running cairn + FF <0.14 and
   stepping up to v0.14 will see both shapes briefly during rollout;
   no data loss, but TTL-expiration of the old-shape keys takes up to
   `lease_ttl_ms * 2` (default 60 s). Plan a quiet window.

## Deferred (future RFCs, not this one)

- Cross-namespace `list_workers` (requires a tuple-cursor RFC).
- Worker-to-lane fencing at the storage layer (RFC-025 §Non-goals 5).
- Per-worker backpressure / in-flight counters (§Non-goals 6).
- Operator-event stream for worker lifecycle (§Non-goals 9).

Track follow-ups in the main FF issue tracker; cite RFC-025 §Non-goals
when filing.
