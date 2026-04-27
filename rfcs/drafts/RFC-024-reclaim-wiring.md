# RFC-024: Wire `ff_reclaim_execution` + `ff_issue_reclaim_grant` to the consumer surface

**Status:** DRAFT
**Revision:** 1
**Author:** FlowFabric Team (manager single-agent draft)
**Proposed:** 2026-04-26
**Target release:** v0.12.0 (same wave as SQLite — Wave 10 folded)
**Related RFCs:** RFC-012 (EngineBackend trait), RFC-017 (ff-server backend abstraction), RFC-018 (capability discovery), RFC-020 (Postgres Wave 9, shipped v0.11.0), RFC-023 (SQLite dev-only backend, v0.12.0 wave)
**Tracking issue:** #371
**Consumer report:** avifenesh/cairn-rs → in-tree drafts `docs/design/ff-upstream/ff-lease-renewal-pull-mode.md`, `ff-renew-lease-mid-approval.md`, `ff-execution-phase-probe.md`, `ff-complete-run-lease-semantics.md`

> **Draft status.** Open questions in §7 list the two residual
> owner forks after investigation. Every "Option A vs B" flagged as
> DECIDED in-draft (§3, §4) is the drafter's recommendation under
> the owner decisions summarised below; each remains open to owner
> override.

### Owner decisions folded into Rev-1

These four decisions were made before drafting and are treated as
load-bearing throughout:

1. **`ReclaimGrant` kind discriminant, not a second type.** The
   existing `ff_core::contracts::ReclaimGrant` gains a `kind:
   GrantKind` field; it does not fork into two separate grant
   types. Additive, grep-able, ergonomic for consumers that want
   one enum arm per path.
2. **`max_reclaim_count` is configurable per-call, default 1000.**
   The Lua primitive already accepts a per-execution policy
   override and falls back to a hard-coded default of `100` at
   `crates/ff-script/src/flowfabric.lua:3036`. The Rust surface
   exposes this as an optional field on `IssueReclaimGrantArgs`
   (pre-existing struct at `crates/ff-core/src/contracts/mod.rs:512`)
   defaulting to **1000** when the caller passes `None`. Rationale:
   long-running interactive cairn sessions (operator-paced tool
   approvals, 30s+ per call) iterate dozens of reclaim cycles per
   run in the degenerate case; 100 is a silent deadlock trap for
   pull-mode consumers who walk away. 1000 turns the trap into a
   loud, classifiable, terminal failure.
3. **Valkey ↔ Postgres parity divergence documented, not hidden.**
   Today `claim_from_reclaim` on Valkey routes through
   `ff_claim_resumed_execution` (the suspend/resume path that gates
   on `attempt_interrupted`); on Postgres it is a bespoke SQL path
   that does NOT gate on `lifecycle_phase`; on SQLite the Phase
   2a.3 port followed the Postgres pattern. The three backends
   produce observably different behaviour under the same consumer
   input, and consumers currently cannot tell which one they have.
   §6 is a required section that names every divergence and states
   the post-RFC-024 converged contract. Silent divergence is
   worst-class UX; naming the gap is a prerequisite for shipping
   the fix.
4. **Wave 10 SQLite folded.** All three backends wire the new
   trait method in the same release (v0.12.0 with RFC-023). No
   deferred-backend phase. Parity matrix updates in the same PR
   series.

---

## 1. Summary

Add a new `EngineBackend` trait method + matching `ff-sdk::admin`
method that wires two already-implemented Lua primitives
(`ff_issue_reclaim_grant` and `ff_reclaim_execution`, both landed
pre-RFC at `crates/ff-script/src/flowfabric.lua:2985` + `:3898`) to
the consumer surface. Closes #371 pull-mode deadlock. Resolves
three-way silent Valkey/PG/SQLite divergence on
`claim_from_reclaim`. Wave 10 scope — ships in v0.12.0 alongside
RFC-023 SQLite.

No new Lua FCALL. No new DB migration. Purely additive trait
method + additive grant-kind discriminant.

---

## 2. Motivation

### 2.1 Pull-mode deadlock (issue #371)

Cairn drives FlowFabric executions one HTTP request at a time,
with operator-paced gaps between calls (30s+ per tool approval).
Under the default 30s lease TTL, a sequence of productive
iterations can leave an execution in a state where:

- `ff_complete_execution` rejects with `lease_expired`: the lease
  has drifted past `now_ms`, `mark_expired` ran, and the lease
  fence no longer matches.
- Cairn retries via `POST /v1/runs/:id/claim` →
  `ff_issue_claim_grant`, which checks `lifecycle_phase ==
  "runnable"` at `crates/ff-script/src/flowfabric.lua:3585`. The
  execution is in `lifecycle_phase = "active"` post-`mark_expired`
  (the attempt is still the same attempt; expiry only cleared the
  lease, not the phase). `ff_issue_claim_grant` returns
  `execution_not_eligible`.

Both recovery doors are locked simultaneously. The execution
cannot progress; the real work it represents (files written,
code compiled on the host filesystem) cannot be persisted as a
terminal write.

### 2.2 The primitives already exist and are correct

The Lua primitives that do the right thing landed before this
RFC:

- **`ff_issue_reclaim_grant`** (`flowfabric.lua:3898`) validates
  `lifecycle_phase == "active"` AND `ownership_state IN
  {"lease_expired_reclaimable", "lease_revoked"}`. It does NOT
  require `runnable`. It issues the same
  `claim_grant` hash shape that `ff_issue_claim_grant` uses —
  consumers that already handle `ClaimGrant` handle this
  identically, modulo the `kind` discriminant this RFC adds.
- **`ff_reclaim_execution`** (`flowfabric.lua:2985`) atomically
  interrupts the old attempt, creates a new attempt + new lease,
  bumps the reclaim counter, and enforces per-execution
  `max_reclaim_count` (default 100 in Lua; 1000 on the Rust
  surface per §Owner-decisions). Wrong-state executions are
  rejected with `execution_not_reclaimable`; exceeded-count
  executions are transitioned to a terminal failure so consumers
  see a loud, classifiable outcome — not a silent spin.

The bug in §2.1 is not a missing primitive; it is an unwired
primitive. No Rust caller exists today.

### 2.3 Why the current Valkey `claim_from_reclaim` is not the fix

`crates/ff-backend-valkey/src/lib.rs:4323` routes
`claim_from_reclaim` through `ff_claim_resumed_execution`, the
suspend/resume FCALL at `flowfabric.lua:5823`. That FCALL gates on
`attempt_interrupted`, i.e. the explicit suspend/resume
lifecycle — not the lease-expired-reclaimable state that the
deadlock produces. For #371 inputs the existing trait method
returns `Ok(None)` (grant-no-longer-available shape) because the
execution is not in an attempt-interrupted state.

This is a real bug on Valkey — the method name advertises reclaim
semantics, the implementation delivers resume semantics. It
predates this RFC (#371 catches it as a consumer-visible
symptom). RFC-024 fixes it by introducing a new trait method with
the correct semantic and retaining `claim_from_reclaim` as-is for
the resume path (the name-vs-semantic issue is a follow-up
addressed in §6.3).

### 2.4 Why Postgres doesn't hit the bug but has a silent divergent path

Postgres `claim_from_reclaim` at
`crates/ff-backend-postgres/src/attempt.rs:286–396` checks the
latest attempt row for a lease that has expired (`expires_at <=
now` or NULL), bumps the epoch, updates `ff_exec_core` to
`lifecycle_phase = 'active'` + `ownership_state = 'leased'`, and
mints a fresh handle. It does NOT gate on `lifecycle_phase`
before acting; it writes the desired phase. Under #371 inputs
Postgres unsticks the execution where Valkey does not. Same
trait method, same consumer call, observably different behaviour.
SQLite's Phase 2a.3 port followed the Postgres pattern and
inherits the same silent divergence from Valkey. Consumers today
cannot tell the backends apart.

### 2.5 Scope posture

This RFC is the **narrowest fix** that resolves #371 and
converges the three backends on a single documented contract. It
is not a scheduler redesign, not a new reclaim-scanner, not an
`attempt_interrupted` unification. Those are larger projects
tracked elsewhere; RFC-024's job is to wire the correct primitive
end-to-end.

---

## 3. Consumer surface

### 3.1 Trait addition — `EngineBackend::issue_reclaim_grant`

Added to `crates/ff-core/src/engine_backend.rs` alongside the
existing scheduler-routed claim surface (row 18, RFC-017 §7):

```rust
#[cfg(feature = "core")]
async fn issue_reclaim_grant(
    &self,
    args: IssueReclaimGrantArgs,
) -> Result<IssueReclaimGrantOutcome, EngineError> {
    Err(EngineError::Unavailable {
        op: "issue_reclaim_grant",
    })
}
```

Default impl returns `Unavailable` so pre-RFC out-of-tree
backends keep compiling (same pattern as
`claim_for_worker`, `subscribe_completion`, RFC-018/019 surface).
All three in-tree backends (Valkey, Postgres, SQLite) override.

`IssueReclaimGrantArgs` already exists at
`crates/ff-core/src/contracts/mod.rs:512`. This RFC extends it
additively:

```rust
pub struct IssueReclaimGrantArgs {
    pub execution_id: ExecutionId,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub lane_id: LaneId,
    #[serde(default)]
    pub capability_hash: Option<String>,
    pub grant_ttl_ms: u64,
    #[serde(default)]
    pub route_snapshot_json: Option<String>,
    #[serde(default)]
    pub admission_summary: Option<String>,
    #[serde(default)]
    pub worker_capabilities: BTreeSet<String>, // new, parity with IssueClaimGrantArgs
    /// Maximum reclaim count before the Lua primitive transitions
    /// the execution to terminal_failed. `None` uses the surface
    /// default of 1000 (see §Owner-decisions #2); explicit
    /// `Some(n)` overrides per-call. The Lua primitive reads the
    /// per-execution policy override first and falls back to the
    /// value passed here — callers who want a lower cap for a
    /// specific lane set a small explicit value.
    #[serde(default)]
    pub max_reclaim_count: Option<u32>, // new, Rev-1
    pub now: TimestampMs,
}
```

`worker_capabilities` is added to the existing struct for parity
with `IssueClaimGrantArgs` (both FCALLs apply identical
capability-matching per the comment at `flowfabric.lua:3884`).
This is additive — the `#[serde(default)]` default is an empty
set, which preserves the pre-RFC behaviour for callers that did
not set it. The Lua FCALL already reads
`ARGV[9] = worker_capabilities_csv`; the Rust shim only ever
passed an empty string, which incidentally worked because no
in-tree caller existed. RFC-024 is the first caller and must pass
the real set.

`IssueReclaimGrantResult` (already in tree) is extended additively
into `IssueReclaimGrantOutcome`:

```rust
pub enum IssueReclaimGrantOutcome {
    /// Grant issued; carries the ReclaimGrant the caller hands to
    /// `claim_from_reclaim_grant` next. Same opacity contract as
    /// the existing `IssueClaimGrantResult::Granted` → `ClaimGrant`
    /// pair.
    Granted(ReclaimGrant),
    /// Execution is not in a reclaimable state (not
    /// `lease_expired_reclaimable` / `lease_revoked`). Maps the Lua
    /// `execution_not_reclaimable` error.
    NotReclaimable { execution_id: ExecutionId, detail: String },
    /// Per-execution `max_reclaim_count` exceeded and the Lua
    /// primitive has transitioned the execution to terminal_failed.
    /// The execution is permanently done; consumers should stop
    /// retrying and surface a structural failure.
    ReclaimCapExceeded { execution_id: ExecutionId, reclaim_count: u32 },
}
```

Rationale for an outcome enum rather than `Result<ReclaimGrant,
EngineError>`:

- `NotReclaimable` is a classifiable state, not an engine error —
  consumers can legitimately observe it (raced with a competing
  reclaimer, execution already terminal). Shape matches
  `IssueClaimGrantResult::Granted` which is also an enum (Rust
  surface uses an enum for forward-compat even when there's one
  variant today).
- `ReclaimCapExceeded` is a terminal, classifiable signal. Hiding
  it inside `EngineError` forces consumers into string-matching
  error detail, which is the UX the consumer drafts (#371 issue
  body) explicitly called out as painful. An enum variant with a
  `reclaim_count` field is greppable and machine-classifiable.

Non-Lua-classifiable faults (transport, unexpected FCALL shape,
serialization) still return `Err(EngineError::*)`.

### 3.2 SDK addition — `FlowFabricAdminClient::issue_reclaim_grant`

Added to `crates/ff-sdk/src/admin.rs` alongside
`claim_for_worker`:

```rust
pub async fn issue_reclaim_grant(
    &self,
    req: IssueReclaimGrantRequest,
) -> Result<IssueReclaimGrantResponse, SdkError>;
```

`IssueReclaimGrantRequest` mirrors `IssueReclaimGrantArgs` 1:1 as
a wire struct (serde-serializable, `#[serde(rename_all =
"camelCase")]` to match the rest of the admin surface). The HTTP
route is **new**: `POST /v1/executions/{execution_id}/reclaim`.
`execution_id` rides the URL path, the rest rides the body.
`percent-encoded` per the `claim_for_worker` precedent at
`admin.rs:140-157`.

Response body mirrors `IssueReclaimGrantOutcome`:

```rust
#[serde(tag = "status", rename_all = "snake_case")]
pub enum IssueReclaimGrantResponse {
    Granted {
        execution_id: ExecutionId,
        partition_key: PartitionKey,
        grant_key: String,
        expires_at_ms: u64,
        lane_id: LaneId,
        kind: GrantKind, // always `GrantKind::Reclaim` for this endpoint
    },
    NotReclaimable { execution_id: ExecutionId, detail: String },
    ReclaimCapExceeded { execution_id: ExecutionId, reclaim_count: u32 },
}
```

Existing HTTP error classifications on the admin client
(`SdkError::AdminApi { status, kind, retryable, ... }`) cover the
out-of-band transport + 5xx cases; the outcome enum handles
FF-level classifiable states.

### 3.3 `GrantKind` discriminant — Owner-decision #1

`ReclaimGrant` (existing type at
`crates/ff-core/src/contracts/mod.rs:186`) gains a `kind:
GrantKind` field:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum GrantKind {
    /// Fresh-claim grant — produced by `ff_issue_claim_grant`,
    /// consumed by `ff_claim_execution`. Semantically: admission
    /// of a runnable execution to a worker.
    Claim,
    /// Reclaim grant — produced by `ff_issue_reclaim_grant`,
    /// consumed by `ff_reclaim_execution`. Semantically: recovery
    /// of a lease-expired-reclaimable (or revoked) execution by a
    /// capable worker.
    Reclaim,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimGrant {
    pub execution_id: ExecutionId,
    pub partition_key: crate::partition::PartitionKey,
    pub grant_key: String,
    pub expires_at_ms: u64,
    pub lane_id: LaneId,
    pub kind: GrantKind, // new, Rev-1
}
```

`ClaimGrant` (the fresh-claim type at `contracts/mod.rs:110`)
also gains the same field; grant types remain separate on the
Rust surface to preserve the lane-carrying asymmetry documented
at `contracts/mod.rs:170-176`. The `kind` field is load-bearing
for downstream consumers that want a single match site across
both grant paths:

```rust
match grant.kind {
    GrantKind::Claim => {
        // worker dispatches ff_claim_execution flow
    }
    GrantKind::Reclaim => {
        // worker dispatches ff_reclaim_execution flow
    }
}
```

**Why on both types, not one sum type.** A `Grant` sum type
(`enum Grant { Claim(ClaimGrant), Reclaim(ReclaimGrant) }`) was
considered and rejected: it would break every existing caller of
`claim_from_grant(ClaimGrant)` on a type that was stable at
v0.11. The field-on-existing-type shape is additive with
`#[non_exhaustive]` on `GrantKind` — callers that construct via
the existing `pub` fields get a compile error pointing at the
missing field (breaking) OR, if we use a builder / constructor
wrapper, migrate with `..Default::default()` (non-breaking on the
struct-literal front). See §8 for the migration shape.

### 3.4 Trait surface — `claim_from_reclaim_grant` wiring

The SDK worker path for consuming a `ReclaimGrant { kind:
Reclaim, .. }` uses the existing
`FlowFabricWorker::claim_from_reclaim_grant` shape
(`ff-sdk/src/worker.rs:1337`) — with one behavioural change: when
the grant's `kind` is `GrantKind::Reclaim`, the worker calls a
new backend trait method `reclaim_execution` (routing to
`ff_reclaim_execution`); when `kind` is `GrantKind::Claim` (the
resume-path semantic that pre-RFC `ReclaimGrant` carried), the
worker continues to call `claim_from_reclaim`
(`ff_claim_resumed_execution`). The two primitives coexist on
separate trait methods; the grant `kind` discriminates which path
the worker dispatches.

Net trait surface added in RFC-024:

```rust
// ff-core::engine_backend — new methods
async fn issue_reclaim_grant(
    &self,
    args: IssueReclaimGrantArgs,
) -> Result<IssueReclaimGrantOutcome, EngineError>;

async fn reclaim_execution(
    &self,
    args: ReclaimExecutionArgs, // existing at contracts/mod.rs:540
) -> Result<ReclaimExecutionOutcome, EngineError>;
```

Both have default impls returning `Unavailable` so pre-RFC
out-of-tree backends keep compiling. All three in-tree backends
override.

`ReclaimExecutionOutcome` (new enum):

```rust
pub enum ReclaimExecutionOutcome {
    Claimed(Handle), // new handle, matches claim_from_reclaim's Handle shape
    NotReclaimable { execution_id: ExecutionId, detail: String },
    ReclaimCapExceeded { execution_id: ExecutionId, reclaim_count: u32 },
    GrantNotFound { execution_id: ExecutionId },
}
```

### 3.5 Capability discovery — RFC-018 flag

A new `Supports::issue_reclaim_grant: bool` flag on the
`Capabilities::supports` matrix. All three in-tree backends
report `true` at v0.12.0. Pre-RFC out-of-tree backends report
`false` via the `Supports::none()` default. Consumer flow:
feature-detect via `capabilities().supports.issue_reclaim_grant`
at connect time; fall back to existing retry + eventual-terminal
logic if absent.

---

## 4. Design

### 4.1 Valkey wiring

Direct forward to `ff_issue_reclaim_grant` at
`flowfabric.lua:3898`. KEYS + ARGV match the FCALL signature
exactly:

```
KEYS[1] = exec_core
KEYS[2] = claim_grant
KEYS[3] = lease_expiry_zset
ARGV[1-9] = execution_id, worker_id, worker_instance_id, lane_id,
            capability_hash, grant_ttl_ms, route_snapshot_json,
            admission_summary, worker_capabilities_csv
```

Key construction reuses the existing `ExecKeyContext` shape at
`crates/ff-backend-valkey/src/lib.rs` (same context as
`ff_issue_claim_grant`, different target function). No new key
schema.

Capability-mismatch behaviour mirrors the Lua doc at
`flowfabric.lua:3884-3896`: on `capability_mismatch` the Lua does
NOT remove the exec from `lease_expiry`, and the RFC-024 surface
returns `IssueReclaimGrantOutcome::NotReclaimable { detail:
"capability_mismatch" }`. There is no Batch-C reclaim scanner
today (flowfabric.lua:3865 TODO); the RFC-024 surface is
consumer-initiated only. A future scheduler-side reclaim scanner
tracked elsewhere is out of scope for this RFC.

`reclaim_execution` forwards to `ff_reclaim_execution` at
`flowfabric.lua:2985` with the 14 keys + 8 args the FCALL
requires. `max_reclaim_count` from the Rust call threads through
as an additional ARGV (10th) — pre-RFC the Lua reads the
per-execution policy override and falls back to a hard-coded
`100`; the RFC changes the Lua fallback to read from a new
`ARGV[9] = default_max_reclaim_count` (Rust surface default
1000), which the Lua uses only when the per-execution policy
override is absent. This is the only Lua-side edit in the RFC;
KEYS shape and success path are unchanged.

### 4.2 Postgres wiring

Two new methods in `crates/ff-backend-postgres/src/attempt.rs`:

- `issue_reclaim_grant(&PgPool, IssueReclaimGrantArgs) -> ...`:
  validates `ff_exec_core.lifecycle_phase = 'active'` AND
  `ownership_state IN ('lease_expired_reclaimable',
  'lease_revoked')` in a single `SELECT ... FOR UPDATE`, then
  `INSERT` into a new `ff_claim_grant` row (same shape as the
  existing fresh-claim grant row, `kind` column set to
  `'reclaim'`). Wraps in `retry_serializable` per the existing
  PG transient-error pattern.
- `reclaim_execution(&PgPool, ReclaimExecutionArgs) -> ...`:
  consumes the `ff_claim_grant` row (delete in the same tx),
  inserts a new `ff_attempt` row (new attempt_index), updates
  `ff_exec_core` to `lifecycle_phase = 'active'` +
  `ownership_state = 'leased'` + `eligibility_state =
  'not_applicable'` + `attempt_state = 'running_attempt'`, bumps
  `lease_reclaim_count` on core, emits the RFC-019 `reclaimed`
  lease event via the outbox, commits, and mints a handle. The
  existing `claim_from_reclaim` at `attempt.rs:286-396` shares
  code structure — the new method factors the lease-bump +
  phase-flip + handle-mint into a shared helper; the two methods
  differ only in the validation predicate and the
  grant-consumption step.

**No new migration.** The `ff_claim_grant` table already exists
(Wave 9 scope); adding `kind TEXT NOT NULL DEFAULT 'claim'` is
done by extending migration `0015_*.sql` (next available
sequence number after v0.11.0's `0014_*`) in the same v0.12.0
PR. Default `'claim'` preserves existing-row semantics for
upgrade correctness; new RFC-024 rows write `'reclaim'` explicitly.

**Convergence with existing `claim_from_reclaim`.** PG's current
`claim_from_reclaim` at `attempt.rs:286` is the internal-reconciler
path (no grant involved — it operates on the attempt row
directly). RFC-024 introduces `reclaim_execution` as the
grant-consuming path. The two coexist; the reconciler path is
unchanged by this RFC (see §6.2 for the parity contract).

### 4.3 SQLite wiring

The Phase 2a.3 `claim_from_reclaim` already ships the SQLite
equivalent of the PG reconciler path (see
`crates/ff-backend-sqlite/src/backend.rs:1328-1430` +
`crates/ff-backend-sqlite/src/queries/lease.rs`). RFC-024 ports
the new grant-based methods using the same pattern:

- `issue_reclaim_grant_impl` constructs
  `ff_backend_sqlite::queries::lease::issue_reclaim_grant` (new
  query fn), runs under `retry_serializable`, and returns the
  outcome.
- `reclaim_execution_impl` mirrors the PG shape: consume grant,
  insert attempt, flip core, emit outbox `reclaimed` lease event
  via the `insert_lease_event` helper (already at `backend.rs:1409`).

Migrations are hand-ported per RFC-023 §4.1 parity-drift lint —
the PG `0015` gets a SQLite sibling `0015_*.sql`. Scanner
supervisor on SQLite stays `N=1` per RFC-023 §4.1; no partition
fan-out. `retry_serializable` wraps the same transient-busy
classifier shape as every other Wave-9 SERIALIZABLE op on SQLite.

### 4.4 Consumer pattern (cairn example)

Pre-RFC-024 deadlock flow:

```
POST /v1/runs/:id/complete → lease_expired
  → consumer retries via POST /v1/runs/:id/claim
    → ff_issue_claim_grant rejects: execution_not_eligible
      (lifecycle_phase != runnable)
        → deadlock
```

Post-RFC-024 recovery flow:

```
POST /v1/runs/:id/complete → lease_expired
  → consumer detects lease_expired class via SdkError::AdminApi.kind
    → POST /v1/executions/:id/reclaim (new endpoint)
      → ff_issue_reclaim_grant returns Granted(ReclaimGrant { kind: Reclaim, .. })
        → worker calls claim_from_reclaim_grant(grant)
          → backend dispatches to reclaim_execution (because kind = Reclaim)
            → ff_reclaim_execution creates new attempt + new lease
              → worker retries terminal write on the fresh lease
                → POST /v1/runs/:id/complete succeeds
```

No changes to the worker's core loop beyond the one additional
match arm on `GrantKind`. The lease-id / attempt-id state that
the worker tracks rotates through the fresh lease the reclaim
minted.

### 4.5 Error classification shape

The new admin endpoint returns structured outcomes for the three
classifiable states (`Granted`, `NotReclaimable`,
`ReclaimCapExceeded`) as 200-status responses with `status`
discriminator in the body, per `admin.rs:302`'s
`RotateWaitpointSecretResponse` precedent. Out-of-band faults
(transport, 5xx, malformed body) surface as
`SdkError::AdminApi` / `SdkError::Http` per the existing
classifier pattern.

Cairn's F64 bridge-retry classifier today pattern-matches
`lease_expired` + `execution_not_eligible` string codes. Post-
RFC-024 the classifier matches `kind` on
`SdkError::AdminApi`:

- `"execution_not_reclaimable"` → structural state, stop retrying
- `"reclaim_cap_exceeded"` → terminal, escalate
- `"execution_not_eligible"` during reclaim path → bug, report

The explicit `reclaim_cap_exceeded` terminal signal is the
replacement for the silent-deadlock pattern — consumers get a
loud, classifiable failure at 1000 reclaims rather than an
unending retry loop.

### 4.6 `max_reclaim_count = 1000` default — why 1000, not 100

The Lua primitive ships with `max_reclaim = 100` at
`flowfabric.lua:3036` as the fallback when no per-execution
policy override is set. That ceiling was chosen for the
batch-C scheduler-driven scanner (crash recovery), where each
reclaim fires within a single scanner tick and 100 reclaims is a
generous upper bound for "worker has crashed many times". In
pull-mode consumer traffic, each reclaim fires at operator
cadence (one per HTTP request after lease expiry) — a 30-minute
interactive session under a 30s lease can realistically iterate
60+ reclaims, and a multi-hour session with operator breaks more.
100 is a silent trap; 1000 is the smallest round number
comfortably above any observed pull-mode sessions the consumer
drafts describe while still catching genuine infinite loops.

Per-call override is the escape valve: consumers who want a
tighter ceiling for a specific lane pass `max_reclaim_count:
Some(50)`; consumers who want a looser ceiling for an
admin-approved session pass `Some(5000)`. The default of 1000 is
what the trap-free majority gets.

---

## 5. Non-goals

1. **NOT a change to `ff_issue_claim_grant` semantics.** The
   fresh-claim path stays lifecycle-phase-gated at
   `flowfabric.lua:3585`. That gate is correct for fresh claims —
   a `runnable` execution is the only state where a fresh-claim
   admission makes sense. Relaxing it (cairn's Option 1) would
   let a reclaim masquerade as a fresh claim and skip the
   attempt-renumbering + reclaim-counter enforcement that
   `ff_reclaim_execution` does.
2. **NOT a change to `ff_claim_resumed_execution` semantics.**
   The suspend/resume path (attempt_interrupted →
   running_attempt) stays as-is. RFC-024 does not unify
   suspend/resume with lease-reclaim; those are distinct lifecycle
   events with distinct invariants.
3. **NOT a new Lua FCALL.** RFC-024 wires existing FCALLs. The
   one Lua edit is §4.1's `ARGV[9]` threading for the
   `default_max_reclaim_count`, which preserves every pre-RFC
   call shape.
4. **NOT a new database migration beyond the `kind` column on
   `ff_claim_grant`.** Both backends' schema is otherwise
   untouched.
5. **NOT a Batch-C-style reclaim scanner.** The scheduler-driven
   periodic reclaim that would recover from worker crashes is
   out of scope — RFC-024's surface is consumer-initiated. The
   Lua TODO at `flowfabric.lua:3865` remains open.
6. **NOT a merger of `claim_from_reclaim` (resume) and
   `reclaim_execution` (lease-reclaim) into one trait method.**
   The two primitives have different invariants and the surface
   keeps them distinct. Future consolidation, if any, is a
   separate RFC.
7. **NOT the name-fix for the Valkey `claim_from_reclaim` →
   `ff_claim_resumed_execution` mislabeling.** The existing
   trait method name is semantically wrong (it advertises reclaim
   and delivers resume), but renaming it is a breaking-API change
   that's trivially scheduled as a v0.13.0 follow-up. RFC-024
   leaves it as-is and adds the correctly-named
   `reclaim_execution` alongside.

---

## 6. Backend parity notes (required — Owner-decision #3)

### 6.1 Divergence inventory (pre-RFC-024)

| Backend  | Method                         | Primitive                                           | Gates on `lifecycle_phase`? | Bug class on #371 inputs |
|----------|--------------------------------|-----------------------------------------------------|-----------------------------|--------------------------|
| Valkey   | `claim_from_reclaim`           | `ff_claim_resumed_execution`                        | Gates on `attempt_interrupted` — not lease-expired | Returns `Ok(None)`; deadlock visible |
| Postgres | `claim_from_reclaim`           | Bespoke SQL (`attempt.rs:286`)                      | No                          | Works; unsticks the execution     |
| SQLite   | `claim_from_reclaim`           | Bespoke SQL (`backend.rs:1328`)                     | No                          | Works; matches PG                 |

The method name is the same, the behaviour is three different
things. Consumers testing against one backend get different
outcomes against another. This is the "silent divergence is
worst-class UX" failure mode.

### 6.2 Convergence contract (post-RFC-024)

| Backend  | Method                      | Primitive                                         | Behaviour on #371 inputs                                                   |
|----------|-----------------------------|---------------------------------------------------|----------------------------------------------------------------------------|
| Valkey   | `issue_reclaim_grant`       | `ff_issue_reclaim_grant`                          | Grants reclaim; reclaim counter bumps; consumer proceeds                    |
| Postgres | `issue_reclaim_grant`       | New bespoke SQL (§4.2)                            | Same outcome; identical enum shape                                          |
| SQLite   | `issue_reclaim_grant`       | New bespoke SQL (§4.3)                            | Same outcome; identical enum shape                                          |
| Valkey   | `reclaim_execution`         | `ff_reclaim_execution`                            | Fresh attempt + fresh lease; consumer retries terminal                      |
| Postgres | `reclaim_execution`         | New bespoke SQL (§4.2)                            | Same outcome; identical enum shape                                          |
| SQLite   | `reclaim_execution`         | New bespoke SQL (§4.3)                            | Same outcome; identical enum shape                                          |

All three backends deliver the same observable contract on the
new methods at v0.12.0. RFC-018 capability snapshot test
(`docs/POSTGRES_PARITY_MATRIX.md` updated to track
`issue_reclaim_grant` + `reclaim_execution` rows) enforces this
mechanically — if any backend flips its `Supports` flag to
`false` in a future release, the test fails and the owner
reviews.

### 6.3 Disposition of the pre-existing `claim_from_reclaim`
divergence

The three existing `claim_from_reclaim` trait implementations
remain semantically divergent after RFC-024 ships. Options:

- **(a) Leave as-is, document it as a known bug, fix in
  v0.13.0.** The Valkey impl at `lib.rs:4323` is measurably
  wrong (advertises reclaim, delivers resume); the PG + SQLite
  impls deliver a PG-flavoured reclaim-reconciler that has no
  Valkey equivalent. Renaming `claim_from_reclaim` →
  `claim_from_resume_grant` on Valkey + adding a PG/SQLite
  reconciler for the same semantic is a separate v0.13.0 PR
  tracked as RFC-025. No consumer break in v0.12.0.
- **(b) Deprecate `claim_from_reclaim` on Valkey immediately
  (v0.12.0) and remove in v0.13.0.** Reduces the window of
  silent divergence but pays a deprecation-cycle cost for a
  method that has no known production consumer (pre-RFC it was
  effectively unreachable for the lease-expired-reclaimable
  state).
- **(c) Rename `claim_from_reclaim` → `claim_from_resume_grant`
  on all three backends in v0.12.0 (breaking).** Fastest
  convergence; pays a breaking-API cost in the same release
  that already ships new breaking surface (RFC-023's
  `ServerError` + `ServerConfig` `#[non_exhaustive]` flips).

**Drafter recommendation: (a).** RFC-024 is already wide enough
(new admin endpoint, new trait methods, new grant-kind
discriminant); adding a breaking rename in the same release
balloons migration impact for consumers. RFC-025 in v0.13.0
does the rename cleanly under a focused deprecation cycle. Open
for owner override in §7.

### 6.4 Migration for existing Valkey consumers

No known consumer relies on the Valkey `claim_from_reclaim`
routing to `ff_claim_resumed_execution` for the
lease-expired-reclaimable case — the method shape returns
`Ok(None)` on those inputs, which is indistinguishable from "no
reclaim grant available". Consumers using
`claim_from_reclaim_grant` for the suspend/resume path continue
to work unchanged because RFC-024's grant-kind dispatch (§3.4)
uses `GrantKind::Claim` as the discriminator for the resume
shape (the existing pre-RFC `ReclaimGrant` was semantically a
resume grant; consumers migrate by setting
`kind = GrantKind::Claim` on construction).

---

## 7. Alternatives rejected

### 7.1 Cairn Option 1 — relax `ff_renew_lease` phase gate

The consumer drafts floated allowing `renew_lease` to succeed
when the execution is in a post-tool sub-phase but the caller
still holds the lease. Rejected: `mark_expired` clears
`current_lease_id`, so the subsequent `renew_lease` would fail
on `stale_lease` before the phase gate ever fires. The bug is
not a gate-relax problem; it is a missing-recovery-path problem.

### 7.2 Cairn Option 2 — `ff_describe_execution_phase` probe FCALL

Adds a read-only FCALL that returns `lifecycle_phase` +
`valid_operation_bitmask`. Diagnostic only — the consumer learns
"reclaim not possible" but has no action to take. Does not unstick
the execution. Rejected as insufficient, though potentially
useful as a future observability addition (separate RFC).

### 7.3 Cairn Option 3 — `ff_complete_with_reclaim` atomic FCALL

A single FCALL that atomically reclaims + completes. Rejected
as over-scoped: ~150 LOC × 3 terminal variants (complete, fail,
cancel) + a new atomic invariant ("terminal write accepted
post-reclaim") that has no precedent in the Lua surface. The
two-FCALL flow (`ff_reclaim_execution` → `ff_complete_execution`)
delivers the same outcome under a well-understood pair of
existing invariants; the atomic version offers no semantic win
and a substantially larger maintenance tax.

### 7.4 Rename `claim_from_reclaim` on Valkey now

Discussed in §6.3. Rejected for v0.12.0 scope reasons.

### 7.5 Merge `ClaimGrant` and `ReclaimGrant` into one `Grant`
sum type

Discussed in §3.3. Rejected as a breaking-API change that would
ripple through every worker consumer. The `kind` discriminant
on both existing types delivers the same classification surface
additively.

### 7.6 Expose `max_reclaim_count` as only a per-execution
policy override (not per-call arg)

The Lua primitive already supports per-execution policy
override. Rejected as sole surface: per-execution policy
requires the consumer to write the policy before issuing the
grant, which adds a round-trip and couples reclaim posture to
execution creation. Per-call arg (with per-execution override
still supported on the Lua side) is the ergonomic win.

### 7.7 Default `max_reclaim_count = 100` (Lua-parity)

Discussed in §Owner-decisions + §4.6. Rejected — silent
deadlock trap for pull-mode consumers.

---

## 8. Migration impact (consumer-facing)

**Mostly additive, with two minor breaking changes on the existing
grant types (`ClaimGrant` + `ReclaimGrant` gain a `kind` field).**

### 8.1 Unchanged

- Every v0.11.0 behaviour for Valkey + Postgres + SQLite on the
  existing trait surface. No pre-RFC call shape changes
  observable outcome.
- `ff_issue_claim_grant` + `ff_claim_execution` +
  `ff_claim_resumed_execution` Lua semantics unchanged.
- Postgres `claim_from_reclaim` (internal reconciler path at
  `attempt.rs:286`) unchanged.
- SQLite `claim_from_reclaim` (matches PG) unchanged.

### 8.2 Breaking at v0.12.0

1. **`ClaimGrant` + `ReclaimGrant` gain a `kind: GrantKind`
   field.** The types are not `#[non_exhaustive]` today
   (`contracts/mod.rs:110` + `:186`). Consumers constructing via
   struct literal (no known production consumer — both types are
   minted by `ff-scheduler` / `ff-sdk` and consumed, not hand-
   constructed) must either add `kind: GrantKind::Claim` /
   `kind: GrantKind::Reclaim` explicitly, or the RFC adds a
   `Default` impl for `GrantKind` that returns
   `GrantKind::Claim` (preserving pre-RFC semantics under
   `..Default::default()` spread).

   Both types gain `#[non_exhaustive]` in the same v0.12.0 PR —
   the attribute addition is paired with the field addition per
   the RFC-023 §8 precedent (own the break once, lock in the
   long-term shape).

### 8.3 Additive (non-breaking)

2. **New trait methods** (`issue_reclaim_grant`,
   `reclaim_execution`) with default impls returning
   `Unavailable`. Pre-RFC out-of-tree backends keep compiling;
   in-tree backends override.
3. **New admin endpoint** `POST /v1/executions/{id}/reclaim` and
   matching `FlowFabricAdminClient::issue_reclaim_grant` SDK
   method. No change to existing endpoints.
4. **New `max_reclaim_count: Option<u32>` field** on
   `IssueReclaimGrantArgs`. The struct is not `#[non_exhaustive]`
   today either (`contracts/mod.rs:512`) but is serde-based with
   every field `#[serde(default)]` or explicitly settable; the
   new field carries `#[serde(default)]` and is absent from pre-
   RFC serialized forms. `IssueReclaimGrantArgs` also gains
   `#[non_exhaustive]` in this PR — the struct has no known
   production consumer (it's only now being wired to the trait
   surface).
5. **New `Supports::issue_reclaim_grant: bool` flag** on the
   RFC-018 capability matrix. Default `false`; in-tree backends
   set `true`.
6. **Lua `ff_reclaim_execution` gains one additional ARGV
   (`default_max_reclaim_count`) threaded from the Rust
   surface.** Backwards-compatible — Lua reads `ARGV[9]` with a
   `or "100"` fallback so pre-RFC call sites (if any existed)
   still parse. In-tree RFC-024 callers pass the new ARGV;
   out-of-tree Lua callers (none known) are unaffected.

### 8.4 Consumer migration path (cairn example)

1. `cargo update -p flowfabric` to v0.12.0.
2. Detect `capabilities().supports.issue_reclaim_grant == true`
   at connect time.
3. On `lease_expired` class from
   `POST /v1/runs/:id/complete`: call
   `admin.issue_reclaim_grant(req).await?` (new) instead of
   falling through to the existing `ff_issue_grant`
   claim-retry path.
4. On `Granted(grant)` response: call
   `worker.claim_from_reclaim_grant(grant).await?` (existing
   shape; dispatches to `reclaim_execution` internally because
   `grant.kind == GrantKind::Reclaim`).
5. On `NotReclaimable` / `ReclaimCapExceeded`: classify and
   terminal-log; do not retry.
6. Remove the existing `F64 bridge-retry` classifier's
   pattern-match on `lease_expired + execution_not_eligible`
   string codes — the new flow produces structured outcomes.

Cairn's in-tree `F64 bridge retry loop` at
`cairn-rs/.../f64_bridge.rs` (marked `remove once FF#371 ships`
per the consumer drafts) retires in the same migration PR.
`docs/CONSUMER_MIGRATION_0.12.md` adds the §"RFC-024 reclaim
wiring" section documenting this sequence.

### 8.5 Struct-literal `ClaimGrant` / `ReclaimGrant`
construction

If a consumer DOES hand-construct either type, the migration is
a one-line addition:

```rust
// before
let grant = ClaimGrant {
    execution_id, partition_key, grant_key, expires_at_ms,
};

// after v0.12.0
let grant = ClaimGrant {
    execution_id, partition_key, grant_key, expires_at_ms,
    kind: GrantKind::Claim, // or GrantKind::Reclaim
};
```

Both types also gain `#[non_exhaustive]` so future additions
don't re-break consumers.

---

## 9. Release readiness (hard gates)

All of the following must be satisfied before v0.12.0 ships.
RFC-024 ships in the same release as RFC-023; the release gate
is joint.

- [ ] All three backends (Valkey, Postgres, SQLite) implement
      `issue_reclaim_grant` + `reclaim_execution` and pass the
      RFC-018 capability-matrix snapshot test with
      `supports.issue_reclaim_grant = true`.
- [ ] `ff-test` suite extended with a deadlock-repro scenario
      (drives the #371 repro shape: long gaps between
      `complete_run` attempts, 50-iter run, 30s TTL) on all
      three backends. Pre-RFC: the scenario deadlocks on
      Valkey, unsticks on PG/SQLite. Post-RFC: the scenario
      recovers via the new endpoint on all three with identical
      observable outcomes.
- [ ] Cairn-fabric integration test migrates from the `F64
      bridge retry` shape to the new `issue_reclaim_grant` shape;
      PR merged into cairn-rs tree.
- [ ] `docs/POSTGRES_PARITY_MATRIX.md` gains two rows
      (`issue_reclaim_grant`, `reclaim_execution`) with all
      three backends marked `supported` at v0.12.0.
- [ ] `docs/CONSUMER_MIGRATION_0.12.md` §"RFC-024 reclaim
      wiring" section written, reviewed, and linked from
      CHANGELOG.
- [ ] `CHANGELOG.md [Unreleased] ### Added`: new trait methods,
      new admin endpoint, new SDK method, new `GrantKind` enum,
      new `Supports::issue_reclaim_grant` flag.
      `### Changed`: `ClaimGrant` + `ReclaimGrant` +
      `IssueReclaimGrantArgs` gain `#[non_exhaustive]`; both
      grant types gain `kind` field.
- [ ] `scripts/smoke-sqlite.sh` + the SQLite scenario in
      `scripts/published-smoke.sh` extended with a reclaim
      round-trip (issue grant → reclaim execution → complete
      on the fresh lease). Must pass before tag (per
      `feedback_smoke_before_release`).
- [ ] `scripts/smoke-v0.7.sh` or successor extended with the
      same reclaim round-trip against Valkey + Postgres.
- [ ] Per-backend integration test: reclaim counter hits the
      default (1000) and returns
      `ReclaimCapExceeded { reclaim_count: 1000 }` on all
      three backends; execution transitions to
      terminal_failed as the Lua primitive + PG/SQLite ports
      enforce.
- [ ] `examples/` gets a new `reclaim-pull-mode/` example
      (~150 LOC per CLAUDE.md §5) demonstrating the cairn
      pull-mode recovery flow end-to-end. This is the new-
      example requirement for v0.12.0 headlines.
- [ ] RFC-018 capability snapshot JSON regenerated on the three
      in-tree backends.
- [ ] `release.yml` + `release.toml` + `RELEASING.md` — no new
      publishable crate is introduced by this RFC, so the
      publish-list is unchanged. Confirmed in the release PR
      body.

Per `feedback_smoke_before_release`: smoke runs before tag,
never after.

---

## 10. Maintenance tax commitment

- **Two new trait methods** on `EngineBackend`. Every future
  backend adds two impls; RFC-018 flags make the cost
  machine-checkable.
- **One new admin HTTP route** with a new body shape. Standard
  ff-server route-table addition; no new auth posture (inherits
  existing admin-bearer).
- **One additional ARGV on `ff_reclaim_execution`.** Trivial;
  the Lua primitive already had the framework for it (policy
  override path).
- **One `kind` field on each grant type.** Trivial on
  consumers once migrated; the migration pain is one-time.
- **Cross-backend reclaim-semantic divergence** (§6.1) does
  NOT retire under RFC-024 alone — the existing
  `claim_from_reclaim` stays as-is per §6.3 disposition (a).
  RFC-025 in v0.13.0 retires it cleanly. Interim maintenance
  cost: the RFC-018 matrix has two method rows ("reclaim-via-
  grant" and "reclaim-via-reconciler") until RFC-025 collapses
  them.
- **Documentation drift.** Three docs paths carry RFC-024
  references: `POSTGRES_PARITY_MATRIX.md`,
  `CONSUMER_MIGRATION_0.12.md`, CHANGELOG. Any future rename /
  shape change sweeps all three.

Owner has implicitly accepted this tax via the four
Owner-decisions at the head. This section is the honest ledger.

---

## 11. Open questions (genuine forks for owner adjudication)

Two remain after the drafter's investigation + the four Owner-
decisions already folded.

### 11.1 §6.3 — disposition of the existing `claim_from_reclaim`
Valkey-vs-PG divergence

Three options (a / b / c) enumerated in §6.3. Drafter
recommends **(a)** — leave as-is, retire in RFC-025 under a
focused deprecation cycle. Open for owner override; (c) is
viable if the owner prefers a single breaking release.

### 11.2 `GrantKind::Default` — `Claim` or require explicit?

Per §8.2 / §8.5, an `impl Default for GrantKind` returning
`GrantKind::Claim` makes struct-literal consumers
migrate via `..Default::default()` spread instead of an
explicit field addition. Trade-off:

- **A — `Default = Claim`.** Non-breaking for struct-literal
  consumers under the spread pattern; but hides the grant-kind
  decision from new code, which is exactly the discriminant the
  RFC wants consumers to think about.
- **B — No `Default` impl.** Consumers must set `kind`
  explicitly; compile error pinpoints every construction site.
  Slightly more migration pain; better ergonomics long-term.

Drafter recommends **B** — the whole point of the
discriminant is to force consumers to reason about which path
they're on. Open for owner override.

---

## 12. References

- Issue **#371** — tracking (pull-mode deadlock repro shape)
- **RFC-023** (SQLite dev-only backend, v0.12.0 wave, accepted
  2026-04-26) — release co-wave
- **RFC-020** (Postgres Wave 9, shipped v0.11.0 2026-04-26) —
  `claim_from_reclaim` reconciler precedent
- **RFC-018** capability discovery — new flag target
- **RFC-017** ff-server backend abstraction — trait surface
  precedent
- `crates/ff-script/src/flowfabric.lua:2985` —
  `ff_reclaim_execution` Lua primitive
- `crates/ff-script/src/flowfabric.lua:3898` —
  `ff_issue_reclaim_grant` Lua primitive
- `crates/ff-script/src/flowfabric.lua:3585` — the
  `execution_not_eligible` gate that the pull-mode flow
  currently hits
- `crates/ff-backend-valkey/src/lib.rs:4323` — existing
  Valkey `claim_from_reclaim` (routes to resume path, bug)
- `crates/ff-backend-postgres/src/attempt.rs:286-396` — PG
  reconciler `claim_from_reclaim` (no lifecycle gate)
- `crates/ff-backend-sqlite/src/backend.rs:1328-1430` —
  SQLite Phase 2a.3 `claim_from_reclaim`
- `crates/ff-core/src/contracts/mod.rs:512` —
  `IssueReclaimGrantArgs` (existing, extended Rev-1)
- `crates/ff-core/src/contracts/mod.rs:110,186` — `ClaimGrant`
  + `ReclaimGrant` types (both gain `kind` field)
- `crates/ff-sdk/src/admin.rs:125-201` — `claim_for_worker`
  SDK precedent (route shape, error classification)
- Cairn-rs consumer drafts under
  `docs/design/ff-upstream/` — ff-lease-renewal-pull-mode.md,
  ff-renew-lease-mid-approval.md, ff-execution-phase-probe.md,
  ff-complete-run-lease-semantics.md
