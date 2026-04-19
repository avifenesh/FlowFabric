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
| 3 | Worker claim flow (replace `insecure-direct-claim`) | Rewrite `claim_next`-based code to grant-based path |
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
grep -rn 'ExecutionId::new\|ExecutionId::from_uuid\|ExecutionId::default' src/
```

Every hit is a migration site. If the context has a flow_id → `for_flow`.
Otherwise → `solo`.

**Parsing unchanged:** `ExecutionId::parse(s: &str)` still exists and is
backward-compatible for wire-format reads (FCALL return values, HTTP bodies).
Validation rejects bare UUID strings; see RFC-011 §2.3.1 for the range-check
delegation contract.

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

## Step 3 — Worker claim flow (replace `insecure-direct-claim`)

**FF change:** two new public entry points on `FlowFabricWorker` replace the
`insecure-direct-claim` feature-gated direct-FCALL path. They acquire the
worker's concurrency permit before the FCALL so a saturated worker never
consumes a grant.

```rust
impl FlowFabricWorker {
    /// Consume a scheduler-issued ClaimGrant → ClaimedTask.
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
        // Retryable; re-try after a task completes.
    }
    Err(SdkError::Script(ScriptError::ClaimGrantExpired)) => {
        // Grant TTL elapsed — request a new grant.
    }
    Err(e) => return Err(e.into()),
}
```

**Resume path:** mirror with `claim_from_reclaim_grant` when FF returns
`use_claim_resumed_execution`. The reclaim grant flow is symmetric.

**New error variant:** `SdkError::WorkerAtCapacity` is retryable. The old
`insecure-direct-claim` path returned `Ok(None)` on saturation; the new path
returns `Err(WorkerAtCapacity)` with `is_retryable() == true` so callers can
opt into structured retry. Both `claim_from_grant` and
`claim_from_reclaim_grant` return this variant under saturation.

**Retiring the feature flag:** after this step, your crate can drop
`insecure-direct-claim` from its `ff-sdk` feature list and the associated
`#[cfg(feature = "insecure-direct-claim")]` branches. The ungated grant path
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
use ff_sdk::task::ReportUsageResult;

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
// Returns "ff:usagededup:{hash_tag}:{dedup_id}"
// IMPORTANT: hash_tag must already include braces (e.g. "{bp:7}").
// Don't pre-wrap; the helper does not add braces.
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

## Step 6 — Valkey 8.0 minimum (future, phase 5)

**Status:** not yet landed. RFC-011 §13 requires Valkey 8.0+. Phase 5 adds a
boot-time version check in ff-server and ff-sdk's admin client that fails
fast if the detected version is below 8.0.

**Action:** if your production Valkey is on 7.x, schedule an upgrade before
pulling the phase-5 release. FlowFabric's reference deployment (cairn
integration tests) already pins `valkey/valkey:8-alpine`.

A 60-second exponential-backoff retry budget in the boot-check tolerates
rolling Valkey upgrades — you don't need downtime coordination, but both
versions must be ≥ 8.0 during the rolling window.

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

grep -rn '"insecure-direct-claim"' Cargo.toml
# expect: feature flag removed from your ff-sdk dep

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
- Team discussion: `benches/perf-invest/rfc-exec-colocation-debate.md`
  (full cross-review transcript)
- Phase-3 atomicity tests: `crates/ff-test/tests/e2e_flow_atomicity.rs`
  (pattern reference for consumer-side integration testing)

## Questions / gaps

If you hit a migration edge case not covered here, the FF team tracks
consumer-facing API gaps as GitHub issues labelled `consumer-ergonomics`.
The most likely gap class is missing SDK helpers — if a consumer pattern
takes more than 5 lines, open an issue and FF will evaluate a helper for
a follow-up minor release.
