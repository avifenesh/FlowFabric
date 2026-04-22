# RFC-011 consumer migration guide

FlowFabric landed [RFC-011](../rfcs/RFC-011-exec-flow-colocation.md) across
phases 1-3. This document covers what an SDK consumer (cairn-fabric, or any
future FF consumer) needs to change on their side to adopt the new behavior.

The guide is consumer-agnostic. Every step cites the FF-side change + the
consumer-side action. Follow the steps in order; each is independently
shippable.

**Status as of 2026-04-19:** phases 1-3 merged on `main`. Phase 5 (Valkey 8.0
version floor, operator runbook, partition-collisions probe) is in flight.

## Migration at a glance

| Step | Consumer area | Effort |
|---|---|---|
| 1 | `ExecutionId` construction | 1 line per mint site |
| 2 | `PartitionConfig` field rename | 1 line per field reference + env var rename |
| 3 | Worker claim flow (replace `direct-valkey-claim`) | Rewrite `claim_next`-based code to grant-based path |
| 4 | `parse_report_usage_result` | Delete parallel parser, import FF's |
| 5 | `usage_dedup_key` helper | Replace hardcoded `format!` sites |
| 6 | Valkey 8.0 minimum (future, phase 5) | Upgrade Valkey if on 7.x |

## Step 1 — `ExecutionId` construction

**FF change:** `ExecutionId::new()`, `::from_uuid()`, and `impl Default` are
removed. Construction is now via one of two flow-aware mint paths so the
resulting id's hash-tag pre-selects the co-located partition.

**Before:**

```rust
let eid = ExecutionId::new();
let eid = ExecutionId::from_uuid(some_uuid);
let eid: ExecutionId = Default::default();
```

**After:**

```rust
use ff_core::types::{ExecutionId, FlowId, LaneId};
use ff_core::partition::PartitionConfig;

// Member of a flow — id co-locates with flow's partition
let eid = ExecutionId::for_flow(&flow_id, &config);

// Not a member of any flow — solo exec, lane-derived partition
let eid = ExecutionId::solo(&lane_id, &config);
```

**Why:** under RFC-011, `exec_core` keys live on the same Valkey hash slot as
their parent flow (or lane, for solo execs). The mint path encodes the chosen
partition in the `{fp:N}:<uuid>` hash tag so reads and writes land on the
correct node without a routing round-trip.

**How to audit your code:**

```bash
# Constructor calls
grep -rn 'ExecutionId::new\|ExecutionId::from_uuid' src/

# Default::default() calls that resolve to ExecutionId — the type-annotated
# form and the explicit path form. Also catches #[derive(Default)] on structs
# that included ExecutionId before RFC-011 (they'll fail to derive now).
grep -rn 'ExecutionId::default\|: ExecutionId = Default::default\|#\[derive.*Default' src/
```

Every hit is a migration site. If the context has a flow_id → `for_flow`.
Otherwise → `solo`.

**Parse signature:** `ExecutionId::parse(s: &str)` entry point is unchanged,
but the return error type changed post-RFC-011. The old `uuid::Error` is now
`ExecutionIdParseError` (defined at `ff_core::types`). Consumers that match on
the error type need the rename:

```rust
// Before
fn parse(s: &str) -> Result<ExecutionId, uuid::Error>;

// After
fn parse(s: &str) -> Result<ExecutionId, ExecutionIdParseError>;
```

Validation is stricter: bare UUID strings are now rejected (shape must be
`{fp:N}:<uuid>`). See RFC-011 §2.3.1 for the range-check delegation contract.

## Step 2 — `PartitionConfig` field rename

**FF change:** `num_execution_partitions` is retired. `num_flow_partitions`
is authoritative for both execution and flow routing. Default bumped to 256
(was 64) to preserve fanout under co-location.

**Before:**

```rust
let cfg = PartitionConfig {
    num_execution_partitions: 64,
    num_flow_partitions: 64,
    num_budget_partitions: 32,
    num_quota_partitions: 32,
};
```

**After:**

```rust
let cfg = PartitionConfig {
    num_flow_partitions: 256,  // or whatever value you need; default is 256
    num_budget_partitions: 32,
    num_quota_partitions: 32,
};
```

**Env var rename:** `FF_EXEC_PARTITIONS` → `FF_FLOW_PARTITIONS`. Operators
who previously set `FF_EXEC_PARTITIONS=N` must rename to
`FF_FLOW_PARTITIONS=N` or accept the new default of 256.

**Partition-family note:** `PartitionFamily::Execution` is retained as a
routing alias of `PartitionFamily::Flow` (RFC-011 §11 non-goal). Code that
constructs `Partition { family: PartitionFamily::Execution, ... }` continues
to work; both variants produce the `{fp:N}` hash tag and route via
`num_flow_partitions`.

## Step 3 — Worker claim flow (replace `direct-valkey-claim`)

> **Rename note:** the feature flag previously named
> `insecure-direct-claim` was hard-renamed to `direct-valkey-claim`
> (see Batch C PR-A). If your `Cargo.toml` or source still references
> `insecure-direct-claim`, update to `direct-valkey-claim` before
> following the rest of this step.

**FF change:** three public entry points on `FlowFabricWorker` replace the
`direct-valkey-claim` feature-gated direct-FCALL path. They acquire the
worker's concurrency permit before the FCALL so a saturated worker never
consumes a grant.

The new default production path is **`claim_via_server`** — issued via the
server's `POST /v1/workers/{id}/claim` endpoint. That endpoint runs the
full `ff_scheduler::Scheduler::claim_for_worker` admission cycle (budget
breach, quota sliding-window, capability match) before minting the grant.
The SDK method fetches the grant and chains to `claim_from_grant` so the
lease is still minted on the worker's own Valkey client. Consumers that
already have a `FlowFabricAdminClient` for other admin endpoints just
pass it in — no second HTTP client needed.

```rust
impl FlowFabricWorker {
    /// Production entry point — scheduler-routed claim.
    /// POSTs /v1/workers/{id}/claim, parses the ClaimGrant, chains
    /// to claim_from_grant. Returns Ok(None) on HTTP 204 (no eligible
    /// execution this cycle).
    pub async fn claim_via_server(
        &self,
        admin: &FlowFabricAdminClient,
        lane: &LaneId,
        grant_ttl_ms: u64,
    ) -> Result<Option<ClaimedTask>, SdkError>;

    /// Low-level entry — consume a caller-provided ClaimGrant.
    /// Useful if the caller has its own scheduler invocation path.
    /// Errors: WorkerAtCapacity (retryable), Script(ClaimGrantExpired|...).
    pub async fn claim_from_grant(
        &self,
        lane: LaneId,
        grant: ff_core::contracts::ClaimGrant,
    ) -> Result<ClaimedTask, SdkError>;

    /// Consume a ReclaimGrant (for attempt_interrupted resumptions) → ClaimedTask.
    pub async fn claim_from_reclaim_grant(
        &self,
        grant: ff_core::contracts::ReclaimGrant,
    ) -> Result<ClaimedTask, SdkError>;
}
```

`ClaimGrant` and `ReclaimGrant` are in `ff_core::contracts` (re-exported from
`ff_scheduler` for backward compatibility):

```rust
pub struct ClaimGrant {
    pub execution_id: ExecutionId,
    pub partition: Partition,
    pub grant_key: String,
    pub expires_at_ms: u64,
}

pub struct ReclaimGrant {
    pub execution_id: ExecutionId,
    pub partition: Partition,
    pub grant_key: String,
    pub expires_at_ms: u64,
    pub lane_id: LaneId,  // carried so consumer saves an HGET on the hot path
}
```

**Typical production flow:**

```rust
// Scheduler side: issue a grant for a worker with matching capabilities.
let grant = scheduler.claim_for_worker(&lane, &worker_caps).await?;

// Worker side: consume the grant, atomically claim the execution.
match worker.claim_from_grant(lane.clone(), grant).await {
    Ok(task) => { /* process, eventually call task.complete/fail */ }
    Err(SdkError::WorkerAtCapacity) => {
        // Retryable; retry after a task completes.
    }
    Err(SdkError::Engine(ref boxed))
        if matches!(
            **boxed,
            EngineError::Contention(ContentionKind::ClaimGrantExpired),
        ) =>
    {
        // Grant TTL elapsed — request a new grant.
    }
    Err(e) => return Err(e.into()),
}
```

**Resume path:** mirror with `claim_from_reclaim_grant` when FF returns
`use_claim_resumed_execution`. The reclaim grant flow is symmetric.

**New error variant:** `SdkError::WorkerAtCapacity` is retryable. The old
`direct-valkey-claim` path returned `Ok(None)` on saturation; the new path
returns `Err(WorkerAtCapacity)` with `is_retryable() == true` so callers can
opt into structured retry. Both `claim_from_grant` and
`claim_from_reclaim_grant` return this variant under saturation.

**Retiring the feature flag:** after this step, your crate can drop
`direct-valkey-claim` from its `ff-sdk` feature list and the associated
`#[cfg(feature = "direct-valkey-claim")]` branches. The ungated grant path
is always available.

## Step 4 — `parse_report_usage_result` (drop your parallel parser)

**FF change:** the wire-format parser for `ff_report_usage_and_check` is
now public.

```rust
// ff-sdk
pub fn parse_report_usage_result(
    raw: &ferriskey::Value,
) -> Result<ReportUsageResult, SdkError>;

pub enum ReportUsageResult {
    Ok,
    SoftBreach { dimension: String, current_usage: u64, soft_limit: u64 },
    HardBreach { dimension: String, current_usage: u64, hard_limit: u64 },
    AlreadyApplied,
}
```

**Action:** delete your own copy of this parser. Import the public one.
Prevents format drift between your copy and the producer.

```rust
use ff_sdk::task::parse_report_usage_result;
use ff_core::contracts::ReportUsageResult;  // defined in ff-core, not ff-sdk

match parse_report_usage_result(&raw_fcall_result)? {
    ReportUsageResult::Ok => { /* ... */ }
    ReportUsageResult::SoftBreach { dimension, current_usage, soft_limit } => { /* ... */ }
    ReportUsageResult::HardBreach { .. } => { /* ... */ }
    ReportUsageResult::AlreadyApplied => { /* ... */ }
}
```

## Step 5 — `usage_dedup_key` helper (drop hardcoded `format!`)

**FF change:** shared const + helper live in `ff_core::keys`.

```rust
pub const USAGE_DEDUP_KEY_PREFIX: &str = "ff:usagededup:";

pub fn usage_dedup_key(hash_tag: &str, dedup_id: &str) -> String;
// Returns "ff:usagededup:<hash_tag>:<dedup_id>" where <hash_tag> is inserted
// VERBATIM (the literal braces in "{bp:7}" become part of the key — the
// helper does NOT wrap the input in additional braces).
// Example: usage_dedup_key("{bp:7}", "abc") → "ff:usagededup:{bp:7}:abc"
```

**Before:**

```rust
let key = format!("ff:usagededup:{}:{}", hash_tag, dedup_id);
```

**After:**

```rust
use ff_core::keys::usage_dedup_key;

let key = usage_dedup_key(hash_tag, dedup_id);
```

**Action:** replace every hardcoded `format!("ff:usagededup:...")` in your
codebase. Eliminates drift if the keyspace convention ever changes.

## Step 6 — Valkey 7.2 minimum (landed, phase 3; amended post-phase-3)

**Status:** landed. RFC-011 §13 (as amended in Amendment F, floor-revert)
requires Valkey 7.2+. `ff-server` performs a boot-time version check and
fails fast if the detected `(major, minor)` is below `(7, 2)` with a typed
`ServerError::ValkeyVersionTooLow { detected, required }`.

**Action:** if your production Valkey is on 7.0 / 7.1 (or pre-7), schedule an
upgrade before pulling a post-amendment release. If you are already on 7.2+
(including any 8.x), no action required. FlowFabric's reference deployment
(cairn integration tests) pins `valkey/valkey:8-alpine`; CI also exercises
`valkey/valkey:7.2` to guard the floor.

A 60-second exponential-backoff retry budget in the boot-check tolerates
rolling Valkey upgrades — you don't need downtime coordination, but all
connected nodes must reach ≥ 7.2 within the 60s window.

## Verification checklist

After completing steps 1-5 (step 6 is future-dated):

```bash
# 1. Code audit
grep -rn 'ExecutionId::new\|ExecutionId::from_uuid\|ExecutionId::default' src/
# expect: zero hits

grep -rn 'num_execution_partitions\|FF_EXEC_PARTITIONS' src/
# expect: zero hits

grep -rn 'ff:usagededup:' src/
# expect: zero raw format! sites (helper uses are fine)

grep -rnE '"(direct-valkey-claim|insecure-direct-claim)"' Cargo.toml
# expect: both names removed from your ff-sdk dep
# (insecure-direct-claim was the pre-Batch-C name; direct-valkey-claim
#  is its post-rename form — neither should survive this migration.)

# 2. Build + test
cargo check --workspace
cargo test --workspace
```

All hits resolved → migration complete.

## What NOT to change

- **`PartitionFamily::Execution`** — retained as a routing alias. Code that
  uses the variant continues to work. Don't mass-rewrite to `::Flow`.
- **`ExecutionId::parse()`** signature — unchanged. Old bare-UUID strings
  are rejected, but every wire payload FF emits post-phase-1 is in the
  `{fp:N}:<uuid>` shape.
- **Test fixtures that set a specific partition** — if you have a test
  harness that needs a fixed partition for deterministic routing, look at
  FF's test-only `TestCluster::new_execution_id_on_partition(index: u16)`
  as a reference pattern. Don't add a public `ExecutionId::on_partition()`
  API to your consumer crate — fork the escape-hatch into your test
  fixtures only (see RFC-011 §5.6).

## Reference

- [RFC-011 exec/flow hash-slot co-location](../rfcs/RFC-011-exec-flow-colocation.md)
  — primary design doc with revisions 3 + 4
- Merged PRs in order: #19, #20, #23, #25, #26, #27, #28, #29
- Phase-3 atomicity tests: `crates/ff-test/tests/e2e_flow_atomicity.rs`
  (pattern reference for consumer-side integration testing)

## Questions / gaps

If you hit a migration edge case not covered here, the FF team tracks
consumer-facing API gaps as GitHub issues labelled `consumer-ergonomics`.
The most likely gap class is missing SDK helpers — if a consumer pattern
takes more than 5 lines, open an issue and FF will evaluate a helper for
a follow-up minor release.
