# RFC-024: Wire `ff_reclaim_execution` + `ff_issue_reclaim_grant` to the consumer surface

**Status:** DRAFT
**Revision:** 2
**Author:** FlowFabric Team (manager single-agent draft)
**Proposed:** 2026-04-26
**Target release:** v0.12.0 (joint wave with RFC-023 SQLite; all three backends folded)
**Related RFCs:** RFC-012 (EngineBackend trait), RFC-017 (ff-server backend abstraction), RFC-018 (capability discovery), RFC-019 (lease events), RFC-020 (Postgres Wave 9), RFC-023 (SQLite dev-only backend)
**Tracking issue:** #371
**Consumer report:** avifenesh/cairn-rs → in-tree drafts under `docs/design/ff-upstream/`

---

## Revision 2 changelog

Rev-2 folds six reviewer lenses' findings against Rev-1 (three parallel
reviewers: A-technical, B-consumer, C-framing) plus four additional
owner decisions locked on 2026-04-26. Seven load-bearing changes from
Rev-1:

1. **Two grant types, not one with a discriminant.** Rev-1's
   `GrantKind` field on `ClaimGrant` + `ReclaimGrant` is removed.
   Instead: rename the existing `ReclaimGrant` → `ResumeGrant`
   (its actual semantic — it already routes to
   `ff_claim_resumed_execution`), and introduce a NEW `ReclaimGrant`
   for the lease-reclaim path. Type IS the kind. The naming
   inversion that made reviewers A + C flag Rev-1 as misleading
   is resolved by swapping the name to the semantic it carries.
2. **Trait method rename + new method.** Existing
   `claim_from_reclaim` → `claim_from_resume_grant` on all three
   backends (honest name for what it already does — resume via
   `ff_claim_resumed_execution` on Valkey; attempt-epoch-bump
   reconciler on PG/SQLite). New `claim_from_reclaim_grant` added
   for the new lease-reclaim path. Both land in v0.12.0, no RFC-025
   deferral.
3. **New `ff_claim_grant` table on Postgres + SQLite.** Rev-1
   relied on PG's pre-existing `ff_claim_grant` table — ground
   truth is that no such table exists; PG stashes claim grants in
   `ff_exec_core.raw_fields.claim_grant` JSON (scheduler.rs:252,
   377). RFC-024 lands a properly-shaped table with discriminator
   column (`kind IN ('claim','reclaim')`), backfills existing JSON
   grants during migration, and flips scheduler.rs read/write
   paths to the table. Parallel SQLite migration ships in the same
   wave (RFC-023 N=1, no partitioning).
4. **PG/SQLite `claim_from_reclaim` divergence from Valkey
   `ff_reclaim_execution` resolved.** Rev-1 did not notice that
   PG's current `claim_from_reclaim` reuses `attempt_index` and
   only bumps `lease_epoch` (attempt.rs:286-396), while Valkey's
   `ff_reclaim_execution` creates a new attempt row and bumps
   `attempt_index`. Consumers reading `current_attempt_index` get
   different numbers per backend. Fix: the renamed
   `claim_from_resume_grant` keeps the existing PG/SQLite epoch-
   bump behaviour (it was always a resume path, the name was the
   bug); the new `reclaim_execution` method on PG/SQLite creates a
   new attempt, bumps epoch, bumps `lease_reclaim_count` —
   matching Valkey `ff_reclaim_execution` semantics.
5. **New `lease_reclaim_count` column on PG + SQLite `ff_exec_core`.**
   Required for §9 release-gate test (reclaim-cap-exceeded at
   1000). Incremented by new `reclaim_execution` impl; Valkey
   already tracks this field in its exec_core hash.
6. **`max_reclaim_count` moves from `IssueReclaimGrantArgs` to
   `ReclaimExecutionArgs`.** Ground truth: the field already lives
   on `ReclaimExecutionArgs` at `contracts/mod.rs:553` (u32,
   default 100). Rev-1 proposed adding it to the wrong struct.
   Rev-2 flips the existing `u32` to `Option<u32>`; `None` → 1000
   (the pull-mode-safe default). Per-call override preserved; Lua
   per-execution policy override preserved (the two-default
   coexistence is explicit: Lua fallback stays 100 for pre-RFC
   call sites; Rust surface default is 1000 for RFC-024 callers).
7. **New `HandleKind::Reclaimed` variant.** The enum is already
   `#[non_exhaustive]` (`backend.rs:102`). Adding `Reclaimed`
   lets downstream `complete`/`fail`/metrics paths distinguish
   fresh (first claim), resumed (resume-after-suspend), and
   reclaimed (lease-reclaim-after-expiry). Additive.

Two Rev-1 residual forks resolved by owner decisions:

- §6.3 (disposition of `claim_from_reclaim`): renamed in v0.12.0
  per change #2 above — no RFC-025 follow-up needed.
- §11.2 (`GrantKind::Default`): moot — `GrantKind` removed per
  change #1.

`§11 Residual forks` is now empty. Rev-2 carries no open owner
questions.

---

## 1. Summary

Add two new `EngineBackend` trait methods (`issue_reclaim_grant`,
`reclaim_execution`) + matching SDK admin surface that wire two
already-implemented Lua primitives (`ff_issue_reclaim_grant` at
`crates/ff-script/src/flowfabric.lua:3898`; `ff_reclaim_execution`
at `:2985`) to the consumer surface. Closes #371 pull-mode
deadlock. Resolves three-way silent Valkey/PG/SQLite divergence
on the existing `claim_from_reclaim` method via a rename +
parallel new method. Ships in v0.12.0 — all three backends in the
same release, multiple PRs allowed (per owner decision: zero
deferrals).

New SQL migrations land on both Postgres and SQLite (`ff_claim_grant`
table + `lease_reclaim_count` column on `ff_exec_core`). No new Lua
FCALL; one existing Lua primitive (`ff_reclaim_execution`) gains one
additional ARGV for the Rust-surface default threading.

---

## 2. Motivation

### 2.1 Pull-mode deadlock (issue #371)

Cairn drives FlowFabric one HTTP request at a time, with
operator-paced gaps between calls (30s+ per tool approval). Under
the default 30s lease TTL, a sequence of productive iterations can
leave an execution in a state where:

- `ff_complete_execution` rejects with `lease_expired`: the lease
  has drifted past `now_ms`, `mark_expired` ran, and the lease
  fence no longer matches.
- Cairn retries via `POST /v1/runs/:id/claim` →
  `ff_issue_claim_grant`, which checks `lifecycle_phase ==
  "runnable"` at `flowfabric.lua:3585`. The execution is in
  `lifecycle_phase = "active"` post-`mark_expired` (the attempt is
  still the same attempt; expiry only cleared the lease, not the
  phase). `ff_issue_claim_grant` returns `execution_not_eligible`.

Both recovery doors are locked simultaneously. The execution
cannot progress; the real work (files written, code compiled on
the host filesystem) cannot be persisted as a terminal write.

The two `ownership_state` values from which the post-RFC reclaim
path is reachable:

- `lease_expired_reclaimable` — reached via `ff_mark_lease_expired_if_due`
  scanner tick when the lease's `expires_at_ms <= now_ms`.
- `lease_revoked` — reached via `ff_revoke_lease` admin path when
  an operator or supervisor explicitly releases a wedged lease.

Both transition `lifecycle_phase` to `active` while leaving the
execution ownerless; both are targets for `ff_issue_reclaim_grant`
admission.

### 2.2 The primitives already exist and are correct

- **`ff_issue_reclaim_grant`** (`flowfabric.lua:3898`) validates
  `lifecycle_phase == "active"` AND `ownership_state IN
  {"lease_expired_reclaimable", "lease_revoked"}`. Issues the same
  `claim_grant` hash shape that `ff_issue_claim_grant` uses.
  Applies the same capability match (`flowfabric.lua:3884`).
- **`ff_reclaim_execution`** (`flowfabric.lua:2985`) atomically
  interrupts the old attempt, creates a new attempt + new lease,
  bumps the reclaim counter, and enforces per-execution
  `max_reclaim_count` (default 100 in Lua today; 1000 on the Rust
  surface per §4.6). Validates grant by `grant.worker_id ==
  args.worker_id` — does NOT require matching `worker_instance_id`
  (consumer implication: see §4.4).

The bug in §2.1 is not a missing primitive; it is an unwired
primitive. No Rust caller exists today.

### 2.3 Why the current Valkey `claim_from_reclaim` is not the fix

`ff-backend-valkey/src/lib.rs:4323` routes `claim_from_reclaim`
through `ff_claim_resumed_execution` (flowfabric.lua:5823). That
FCALL gates on `attempt_interrupted` — the explicit suspend/resume
lifecycle — not the `lease_expired_reclaimable` state that the
deadlock produces. For #371 inputs Valkey `claim_from_reclaim`
returns `Ok(None)`.

The method is misnamed: it advertises reclaim, delivers resume.
Rev-2 addresses the naming directly via the rename in change #2 of
the revision changelog (not deferred to RFC-025).

### 2.4 Why PG/SQLite are silently different from Valkey

PG `claim_from_reclaim` at `attempt.rs:286-396` does NOT gate on
`lifecycle_phase`. It locates the latest attempt row, verifies the
lease has expired (`lease_expires_at_ms <= now` or NULL), bumps
`lease_epoch`, updates `ff_exec_core` to `lifecycle_phase =
'active'` + `ownership_state = 'leased'`, and mints a
`HandleKind::Resumed` handle REUSING the same `attempt_index`.
It does not create a new attempt row. It does not bump a reclaim
counter (the column does not exist on PG).

SQLite's Phase 2a.3 port follows the PG pattern.

Three consequences:

1. Same trait method, same consumer call, observably different
   outcomes across backends (PG/SQLite unstick; Valkey deadlocks).
2. Same trait method, same consumer call, observably different
   `current_attempt_index` values (PG/SQLite keep N; Valkey new
   attempt would be N+1 if it reached the reclaim primitive).
3. `lease_reclaim_count` is unobservable on PG/SQLite — the column
   doesn't exist — so §9's 1000-cap test cannot be written as-is.

Rev-2 resolves all three by renaming the existing
`claim_from_reclaim` to `claim_from_resume_grant` (honest name)
and adding a new `reclaim_execution` that converges all three
backends on the new-attempt + reclaim-count semantics that Valkey
`ff_reclaim_execution` already implements.

### 2.5 Scope posture

RFC-024 is the narrowest fix that resolves #371 AND converges the
three backends on a single documented contract. It renames a
pre-existing misleading method (to honest name), adds a new
primitive-wiring method pair (to fix #371), and lands the SQL
shape (table + column) the new method needs. It is not a
scheduler redesign, not a new reclaim-scanner, not an
`attempt_interrupted` unification. Those are tracked elsewhere.

---

## 3. Consumer surface

### 3.1 Grant types — rename existing, add new

**Rename `ReclaimGrant` → `ResumeGrant`** (at
`ff-core/src/contracts/mod.rs:186`). This type has always
represented the resume-after-suspend semantic (the doc comment
names `ff_claim_resumed_execution` as its consumer; the name was
the bug). Carries `lane_id` per the existing asymmetry with
`ClaimGrant`. Gains `#[non_exhaustive]` + an explicit `new()`
constructor in the same PR.

**Add new `ReclaimGrant`** representing the lease-reclaim
semantic. Routes to `ff_reclaim_execution`. Carries `lane_id`
(needed for the reclaim FCALL's `KEYS[*]` construction — same
rationale as `ResumeGrant`). `#[non_exhaustive]` with explicit
`new()`.

**`ClaimGrant` unchanged** except for `#[non_exhaustive]` +
`new()` (fresh-claim path, routes to `ff_claim_execution`).
No lane_id (the existing asymmetry — admission caller already has
the lane as a separate arg).

```rust
// contracts/mod.rs — renamed
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumeGrant {
    pub execution_id: ExecutionId,
    pub partition_key: PartitionKey,
    pub grant_key: String,
    pub expires_at_ms: u64,
    pub lane_id: LaneId,
}

impl ResumeGrant {
    pub fn new(/* all fields required */) -> Self { … }
    pub fn partition(&self) -> Result<Partition, PartitionKeyParseError> { … }
}

// contracts/mod.rs — new
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimGrant {
    pub execution_id: ExecutionId,
    pub partition_key: PartitionKey,
    pub grant_key: String,
    pub expires_at_ms: u64,
    pub lane_id: LaneId,
}

impl ReclaimGrant {
    pub fn new(/* all fields required */) -> Self { … }
    pub fn partition(&self) -> Result<Partition, PartitionKeyParseError> { … }
}
```

No `GrantKind` discriminant. The type IS the kind; at every
dispatch site consumers match on the type (function arg or enum
variant), not a field.

**Rationale for two types over one with a discriminant.** A single
`ReclaimGrant` with a `GrantKind` field (Rev-1 shape) hides the
semantic divergence inside a runtime check — a consumer reaching
for a reclaim grant and a consumer reaching for a resume grant do
different things downstream (different FCALL, different handle
kind, different invariants on the old attempt). Compile-time
dispatch on the type pins the choice at the type system; the enum
field deferred the choice to a runtime match that every consumer
would write identically. Two types also clean up `HandleKind`'s
mapping: `ResumeGrant` → `HandleKind::Resumed`, `ReclaimGrant` →
`HandleKind::Reclaimed` (new variant, see §3.4).

### 3.2 Trait surface — `EngineBackend`

Three changes on `ff-core/src/engine_backend.rs`:

**Rename** the existing `claim_from_reclaim` method →
`claim_from_resume_grant`. Signature:

```rust
async fn claim_from_resume_grant(
    &self,
    token: ResumeToken, // renamed from ReclaimToken; carries ResumeGrant
) -> Result<Option<Handle>, EngineError>;
```

Semantics unchanged: bumps `lease_epoch`, reuses `attempt_index`,
mints `HandleKind::Resumed`. The rename is a compile break but a
zero-semantic-change migration.

**Add `issue_reclaim_grant`:**

```rust
async fn issue_reclaim_grant(
    &self,
    args: IssueReclaimGrantArgs,
) -> Result<IssueReclaimGrantOutcome, EngineError> {
    Err(EngineError::Unavailable { op: "issue_reclaim_grant" })
}
```

Default impl returns `Unavailable` so pre-RFC out-of-tree backends
keep compiling. All three in-tree backends override.

**Add `reclaim_execution`:**

```rust
async fn reclaim_execution(
    &self,
    args: ReclaimExecutionArgs,
) -> Result<ReclaimExecutionOutcome, EngineError> {
    Err(EngineError::Unavailable { op: "reclaim_execution" })
}
```

Default impl returns `Unavailable`. All three in-tree backends
override; mints `HandleKind::Reclaimed` on success.

`IssueReclaimGrantArgs` (existing, at `contracts/mod.rs:512`) is
extended additively with `worker_capabilities: BTreeSet<String>`
(parity with `IssueClaimGrantArgs` — the FCALL already reads
`ARGV[9]`; pre-RFC Rust callers passed empty because no caller
existed). `#[serde(default)]` preserves wire compat. Gains
`#[non_exhaustive]` + explicit `new()`.

`ReclaimExecutionArgs` (existing, at `contracts/mod.rs:540`) is
modified:

- `max_reclaim_count: u32` (existing, default 100 via
  `default_max_reclaim_count()`) → `max_reclaim_count: Option<u32>`
  (new, `None` means use Rust-surface default of 1000). The Lua
  per-execution policy override still dominates when set;
  unset-policy callers hit 1000 via the Rust surface.
- Gains `#[non_exhaustive]` + explicit `new()`.

`IssueReclaimGrantOutcome`:

```rust
#[non_exhaustive]
pub enum IssueReclaimGrantOutcome {
    /// Grant issued. Hand this to `claim_from_reclaim_grant`.
    Granted(ReclaimGrant),
    /// Execution not in a reclaimable state
    /// (not `lease_expired_reclaimable` / `lease_revoked`).
    NotReclaimable { execution_id: ExecutionId, detail: String },
    /// `max_reclaim_count` exceeded; execution transitioned to
    /// terminal_failed by the Lua/SQL primitive. Consumers stop
    /// retrying and surface a structural failure.
    ReclaimCapExceeded { execution_id: ExecutionId, reclaim_count: u32 },
}
```

`ReclaimExecutionOutcome`:

```rust
#[non_exhaustive]
pub enum ReclaimExecutionOutcome {
    Claimed(Handle),  // HandleKind::Reclaimed
    NotReclaimable { execution_id: ExecutionId, detail: String },
    ReclaimCapExceeded { execution_id: ExecutionId, reclaim_count: u32 },
    GrantNotFound { execution_id: ExecutionId },
}
```

**worker_capabilities source (B-2 resolution).** The capabilities
vector is SDK-owned, plumbed from the registered worker's
`WorkerRegistration::capabilities` at grant issuance. Consumers do
not pass capabilities at each admin call; the SDK reads them from
worker state. Rationale: pull-mode consumer ergonomics (one less
thing to thread through every HTTP handler) and prevents drift
between registered capabilities and per-call-asserted capabilities.
The `IssueReclaimGrantArgs.worker_capabilities` field is populated
by the SDK's `admin.issue_reclaim_grant` method before the trait
dispatch; it is not exposed on the wire request type.

### 3.3 SDK surface — `FlowFabricAdminClient::issue_reclaim_grant`

Added to `ff-sdk/src/admin.rs` alongside `claim_for_worker`:

```rust
pub async fn issue_reclaim_grant(
    &self,
    req: IssueReclaimGrantRequest,
) -> Result<IssueReclaimGrantResponse, SdkError>;
```

HTTP route: `POST /v1/executions/{execution_id}/reclaim`.

```rust
#[serde(tag = "status", rename_all = "snake_case")]
pub enum IssueReclaimGrantResponse {
    Granted {
        execution_id: ExecutionId,
        partition_key: PartitionKey,
        grant_key: String,
        expires_at_ms: u64,
        lane_id: LaneId,
    },
    NotReclaimable { execution_id: ExecutionId, detail: String },
    ReclaimCapExceeded { execution_id: ExecutionId, reclaim_count: u32 },
}
```

No `GrantKind` field — the endpoint always returns a reclaim
grant; consumers construct a `ReclaimGrant` from the `Granted`
variant.

### 3.4 `HandleKind::Reclaimed` variant

`HandleKind` at `ff-core/src/backend.rs:102` is already
`#[non_exhaustive]`. Add variant:

```rust
#[non_exhaustive]
pub enum HandleKind {
    Fresh,        // existing
    Resumed,      // existing — from claim_from_resume_grant
    Suspended,    // existing
    Reclaimed,    // new — from reclaim_execution
}
```

Additive. Downstream `complete`/`fail`/metrics paths that match
exhaustively add a `Reclaimed` arm (or rely on `_ =>` fallthrough
per the `#[non_exhaustive]` contract for untouched consumers).

### 3.5 Worker SDK — `claim_from_reclaim_grant`

The existing `FlowFabricWorker::claim_from_reclaim_grant` method
(`ff-sdk/src/worker.rs:1337`) currently consumes a `ReclaimGrant`
and dispatches to the resume path. Post-rename:

- `FlowFabricWorker::claim_from_resume_grant(ResumeGrant)` —
  existing method, renamed. Dispatches to
  `EngineBackend::claim_from_resume_grant`.
- `FlowFabricWorker::claim_from_reclaim_grant(ReclaimGrant)` —
  new method (shape). Dispatches to
  `EngineBackend::reclaim_execution`.

The pre-RFC method name `claim_from_reclaim_grant` retires its
resume-path semantic; consumers migrate call sites to the new
name per §8.

### 3.6 Capability discovery — RFC-018 flag

New `Supports::issue_reclaim_grant: bool` flag on the
`Capabilities::supports` matrix. All three in-tree backends report
`true` at v0.12.0. Pre-RFC out-of-tree backends report `false` via
the `Supports::none()` default.

---

## 4. Design

### 4.1 Valkey wiring

Direct forward of both new methods to the existing FCALLs:

**`issue_reclaim_grant`** → `ff_issue_reclaim_grant`
(`flowfabric.lua:3898`). KEYS + ARGV match the FCALL signature
exactly:

```
KEYS[1] = exec_core
KEYS[2] = claim_grant
KEYS[3] = lease_expiry_zset
ARGV[1..9] = execution_id, worker_id, worker_instance_id, lane_id,
             capability_hash, grant_ttl_ms, route_snapshot_json,
             admission_summary, worker_capabilities_csv
```

Key construction reuses `ExecKeyContext` (same shape as
`ff_issue_claim_grant`). No new key schema.

**`reclaim_execution`** → `ff_reclaim_execution`
(`flowfabric.lua:2985`). KEYS[1..14] and ARGV[1..8] match the
existing FCALL. **One Lua edit:** add `ARGV[9] =
default_max_reclaim_count`, which the Lua reads as the fallback
when the per-execution policy override (at `:3036-3045`) is
absent. Today the Lua hardcodes `100` as the fallback; post-RFC it
reads `ARGV[9]` with an `or "100"` default (so pre-RFC call sites
with fewer args still parse). The Rust surface passes the
user-supplied `max_reclaim_count.unwrap_or(1000)` or the value the
caller set explicitly.

**Lease-expiry scanner interaction.** On successful reclaim the
Lua primitive ZADDs the execution at the new lease's expiry into
`lease_expiry_zset`; subsequent `mark_lease_expired_if_due`
scanner ticks re-use this entry without special-case logic.

Capability-mismatch behaviour mirrors the Lua doc at
`flowfabric.lua:3884-3896`: on mismatch the Lua does NOT remove
from `lease_expiry`; the Rust surface returns
`IssueReclaimGrantOutcome::NotReclaimable { detail:
"capability_mismatch" }`. There is no Batch-C reclaim scanner
today; RFC-024 is consumer-initiated only. A future scheduler-
side reclaim scanner is tracked elsewhere.

### 4.2 Postgres wiring

**Migration `0015_claim_grant_table.sql`** (next sequence number
after `0014_cancel_backlog.sql`):

```sql
CREATE TABLE ff_claim_grant (
    partition_key   SMALLINT   NOT NULL,
    grant_id        BYTEA      NOT NULL,
    execution_id    BYTEA      NOT NULL,
    kind            TEXT       NOT NULL CHECK (kind IN ('claim','reclaim')),
    worker_id       TEXT       NOT NULL,
    worker_instance_id TEXT    NOT NULL,
    lane_id         TEXT,            -- NULL for fresh-claim grants; set for reclaim
    capability_hash TEXT,
    worker_capabilities JSONB   NOT NULL DEFAULT '[]'::jsonb,
    route_snapshot_json TEXT,
    admission_summary TEXT,
    grant_ttl_ms    BIGINT     NOT NULL,
    issued_at_ms    BIGINT     NOT NULL,
    expires_at_ms   BIGINT     NOT NULL,
    PRIMARY KEY (partition_key, grant_id)
);
CREATE INDEX ix_claim_grant_execution ON ff_claim_grant (partition_key, execution_id);
CREATE INDEX ix_claim_grant_expiry    ON ff_claim_grant (expires_at_ms);

-- Backfill existing JSON-stashed grants
INSERT INTO ff_claim_grant (partition_key, grant_id, execution_id, kind,
                            worker_id, worker_instance_id, lane_id,
                            grant_ttl_ms, issued_at_ms, expires_at_ms,
                            worker_capabilities)
SELECT
    c.partition_key,
    decode(c.raw_fields->'claim_grant'->>'grant_id', 'hex'),
    c.execution_id,
    'claim',
    c.raw_fields->'claim_grant'->>'worker_id',
    c.raw_fields->'claim_grant'->>'worker_instance_id',
    NULL,
    (c.raw_fields->'claim_grant'->>'grant_ttl_ms')::BIGINT,
    (c.raw_fields->'claim_grant'->>'issued_at_ms')::BIGINT,
    (c.raw_fields->'claim_grant'->>'expires_at_ms')::BIGINT,
    '[]'::jsonb
  FROM ff_exec_core c
 WHERE c.raw_fields ? 'claim_grant'
   AND (c.raw_fields->'claim_grant'->>'grant_id') IS NOT NULL;

-- Leave raw_fields.claim_grant in place for one release; scheduler.rs
-- read path writes to the table going forward and stops consulting JSON.
-- Migration 0017 in v0.13.0 strips the JSON residue.

ALTER TABLE ff_exec_core
    ADD COLUMN lease_reclaim_count INTEGER NOT NULL DEFAULT 0;
```

`worker_capabilities` uses JSONB (not `TEXT[]`) for SQLite parity
— SQLite has no array type. Migration folds the `ff_claim_grant`
table AND the `lease_reclaim_count` column into a single file to
minimize sequence-number churn (one PR, one migration file).

**Scheduler.rs updates.** The three-site JSON stash (`scheduler.rs:
35`, `:252`, `:377`) is replaced:

- Write path (`:252`) — `INSERT INTO ff_claim_grant ... VALUES (...,
  'claim', ...)` instead of writing into `ff_exec_core.raw_fields`.
- Read path (`:377`) — `SELECT ... FROM ff_claim_grant WHERE
  partition_key = $1 AND execution_id = $2 AND kind = 'claim'`.
- Delete path (grant consumption) — DELETE from the table.

**New method impls in `attempt.rs`:**

- `issue_reclaim_grant_impl`: validates `ff_exec_core.lifecycle_phase
  = 'active'` AND `ownership_state IN ('lease_expired_reclaimable',
  'lease_revoked')` via `SELECT ... FOR UPDATE`. Applies the
  same capability-match logic as `issue_claim_grant`. On match:
  `INSERT INTO ff_claim_grant (kind='reclaim', lane_id=<from
  core>, ...)`. Wraps in `retry_serializable`.
- `reclaim_execution_impl`: consumes the `kind='reclaim'` grant
  row (DELETE in tx), inserts a **new** `ff_attempt` row with
  `attempt_index = (prev_max + 1)`, updates `ff_exec_core` to
  `lifecycle_phase = 'active'` + `ownership_state = 'leased'` +
  `eligibility_state = 'not_applicable'` + `attempt_state =
  'running_attempt'`, increments `lease_reclaim_count`, checks the
  new count against `max_reclaim_count` (from args; default 1000
  if `None`) — if exceeded, transitions to terminal_failed and
  returns `ReclaimCapExceeded` without minting a handle. Emits
  RFC-019 `reclaimed` lease event via the outbox. Commits. Mints
  `Handle { kind: Reclaimed, .. }`.

**Renamed method.** `claim_from_reclaim` in `attempt.rs:286` is
renamed `claim_from_resume_grant_impl`. Body unchanged.

### 4.3 SQLite wiring

Parallel migration `0015_claim_grant_table.sql` in
`crates/ff-backend-sqlite/migrations/`. Same shape as PG, adjusted
for SQLite types:

```sql
CREATE TABLE ff_claim_grant (
    partition_key   INTEGER NOT NULL,
    grant_id        BLOB    NOT NULL,
    execution_id    BLOB    NOT NULL,
    kind            TEXT    NOT NULL CHECK (kind IN ('claim','reclaim')),
    worker_id       TEXT    NOT NULL,
    worker_instance_id TEXT NOT NULL,
    lane_id         TEXT,
    capability_hash TEXT,
    worker_capabilities TEXT NOT NULL DEFAULT '[]',   -- JSON text
    route_snapshot_json TEXT,
    admission_summary TEXT,
    grant_ttl_ms    INTEGER NOT NULL,
    issued_at_ms    INTEGER NOT NULL,
    expires_at_ms   INTEGER NOT NULL,
    PRIMARY KEY (partition_key, grant_id)
);
CREATE INDEX ix_claim_grant_execution ON ff_claim_grant (partition_key, execution_id);
CREATE INDEX ix_claim_grant_expiry    ON ff_claim_grant (expires_at_ms);

ALTER TABLE ff_exec_core ADD COLUMN lease_reclaim_count INTEGER NOT NULL DEFAULT 0;
```

No partition fan-out (RFC-023 N=1). Backfill follows the same
JSON-extraction shape as PG but using `json_extract` instead of
`->>`. Scanner supervisor stays `N=1`.

Method impls mirror PG: `issue_reclaim_grant_impl`,
`reclaim_execution_impl`, and the rename of
`claim_from_reclaim` → `claim_from_resume_grant_impl`. Each wraps
in `retry_serializable` with the transient-busy classifier from
the Wave-9 pattern.

### 4.4 Consumer pattern (cairn example)

Pre-RFC-024 deadlock flow:

```
POST /v1/runs/:id/complete → lease_expired
  → consumer retries via POST /v1/runs/:id/claim
    → ff_issue_claim_grant rejects: execution_not_eligible
      → deadlock
```

Post-RFC-024 recovery flow:

```
POST /v1/runs/:id/complete → lease_expired
  → consumer detects lease_expired class via SdkError
    → POST /v1/executions/:id/reclaim (new endpoint)
      → ff_issue_reclaim_grant returns Granted(ReclaimGrant)
        → worker.claim_from_reclaim_grant(grant)
          → backend.reclaim_execution
            → ff_reclaim_execution creates new attempt + new lease
              → worker retries terminal write on the fresh lease
                → POST /v1/runs/:id/complete succeeds
```

**Worker-instance-identity contract (B-1 resolution).** The Lua
`ff_reclaim_execution` validates grant consumption via
`grant.worker_id == args.worker_id` at `flowfabric.lua:3088` — it
does NOT compare `worker_instance_id`. So a reclaiming worker
process with a different `worker_instance_id` from the expired
lease's holder can consume a reclaim grant it issued itself. This
is load-bearing for cairn's per-request-spawn model: each HTTP
request spawns a fresh worker process with a fresh
`worker_instance_id`; the reclaim flow must work across
instances. PG/SQLite `reclaim_execution` impls honour the same
contract (grant-consumer's `worker_id` must match the grant's;
`worker_instance_id` on the grant is informational only, written
into the new attempt row as the new lease holder).

### 4.5 Error classification shape

Admin endpoint returns structured outcomes as 200-status responses
with `status` discriminator body (per
`RotateWaitpointSecretResponse` precedent at `admin.rs:302`).
Out-of-band faults (transport, 5xx, malformed body) surface as
`SdkError::AdminApi` / `SdkError::Http`.

### 4.6 `max_reclaim_count = 1000` default — why 1000, not 100

The Lua primitive ships with `max_reclaim = 100` at
`flowfabric.lua:3036` as the fallback when no per-execution policy
override is set. That ceiling suits the batch-C scanner
(scheduler-driven crash recovery, many reclaims per scanner tick,
100 = "worker has crashed many times"). In pull-mode consumer
traffic each reclaim fires at operator cadence (one per HTTP
request after lease expiry) — a 30-minute interactive session at
30s TTL can realistically iterate 60+ reclaims; multi-hour
sessions more. 100 is a silent trap; 1000 is the smallest round
number comfortably above observed pull-mode sessions while still
catching genuine infinite loops.

**Two-default coexistence, explicit.** The Lua-side fallback stays
100 (for pre-RFC call sites that may construct a
`ReclaimExecutionArgs` without the new field — `Option<u32> =
None` on deserialize old wire shape; the Lua's `or "100"` on
ARGV[9] catches the case of an old Rust caller omitting the
arg). The Rust surface default is 1000 for any new construction
via the new `ReclaimExecutionArgs::new()` builder or explicit
`None`. This is dissonant by design: the Lua's 100 is the
scheduler-scanner ceiling; the Rust's 1000 is the pull-mode
consumer ceiling. A periodic warn-log fires at `reclaim_count %
100 == 0` so operators observe long-running reclaim sessions
before hitting the terminal cap.

**Per-call override** remains the escape valve: consumers set
`Some(n)` for a tighter (or looser) ceiling specific to a lane.

---

## 5. Non-goals

1. **NOT a change to `ff_issue_claim_grant` semantics.** Fresh
   claims stay `lifecycle_phase`-gated at `flowfabric.lua:3585`.
   Relaxing it (cairn's Option 1 — see §7) would let a reclaim
   masquerade as a fresh claim and skip attempt-renumbering +
   reclaim-counter enforcement.
2. **NOT a change to `ff_claim_resumed_execution` semantics.** The
   suspend/resume path (attempt_interrupted → running_attempt)
   stays as-is. RFC-024 does not unify suspend/resume with
   lease-reclaim.
3. **NOT a new Lua FCALL.** One existing FCALL
   (`ff_reclaim_execution`) gains one additional ARGV for
   configurable max-reclaim-count default threading. All other
   FCALL shapes unchanged.
4. **NOT a Batch-C-style reclaim scanner.** The scheduler-driven
   periodic reclaim that would recover from worker crashes is out
   of scope — RFC-024's surface is consumer-initiated. The Lua
   TODO at `flowfabric.lua:3865` remains open.
5. **NOT a merger of resume and reclaim trait methods.** The two
   primitives have different invariants (resume re-uses attempt;
   reclaim creates new attempt). The surface keeps them distinct
   via two trait methods + two grant types.
6. **NOT a `ff_exec_core.raw_fields.claim_grant` JSON removal.**
   Migration `0015` backfills JSON → table but leaves the JSON in
   place for one release. A follow-up migration in v0.13.0
   strips the JSON residue once all consumers have caught up.

---

## 6. Backend parity notes

### 6.1 Divergence inventory (pre-RFC-024)

| Backend  | Method                | Primitive                        | Attempt-index on return | Reclaim count tracked? | Bug class on #371 inputs |
|----------|-----------------------|----------------------------------|-------------------------|------------------------|--------------------------|
| Valkey   | `claim_from_reclaim`  | `ff_claim_resumed_execution`     | Reuses (attempt_interrupted gate fails anyway) | N/A (method doesn't reach reclaim path) | Returns `Ok(None)`; deadlock |
| Postgres | `claim_from_reclaim`  | Bespoke SQL (`attempt.rs:286`)   | Reuses `attempt_index`; bumps epoch only       | No column exists             | Works; unsticks but under resume semantics |
| SQLite   | `claim_from_reclaim`  | Bespoke SQL (`backend.rs:1328`)  | Reuses `attempt_index`; bumps epoch only       | No column exists             | Works; matches PG |

Same method name, three different semantics (Valkey resume-only,
PG/SQLite epoch-bump-only reconciler). Consumers observing
`current_attempt_index` or querying a reclaim counter get
different answers per backend.

### 6.2 Convergence contract (post-RFC-024)

| Backend  | Method                     | Primitive                                | Attempt-index | Reclaim count | Handle kind |
|----------|----------------------------|------------------------------------------|---------------|---------------|-------------|
| Valkey   | `claim_from_resume_grant`  | `ff_claim_resumed_execution`             | Reuses        | N/A (resume)  | `Resumed`   |
| Postgres | `claim_from_resume_grant`  | Bespoke SQL (attempt.rs, renamed)        | Reuses        | N/A (resume)  | `Resumed`   |
| SQLite   | `claim_from_resume_grant`  | Bespoke SQL (backend.rs, renamed)        | Reuses        | N/A (resume)  | `Resumed`   |
| Valkey   | `issue_reclaim_grant`      | `ff_issue_reclaim_grant`                 | —             | —             | (no handle) |
| Postgres | `issue_reclaim_grant`      | New SQL (ff_claim_grant insert)          | —             | —             | (no handle) |
| SQLite   | `issue_reclaim_grant`      | New SQL (ff_claim_grant insert)          | —             | —             | (no handle) |
| Valkey   | `reclaim_execution`        | `ff_reclaim_execution`                   | New (bumped)  | Bumped        | `Reclaimed` |
| Postgres | `reclaim_execution`        | New SQL (attempt.rs)                     | New (bumped)  | Bumped        | `Reclaimed` |
| SQLite   | `reclaim_execution`        | New SQL (backend.rs)                     | New (bumped)  | Bumped        | `Reclaimed` |

All three backends deliver identical observable contracts on the
new methods at v0.12.0. No residual divergence.

### 6.3 Migration for existing `claim_from_reclaim` callers

The rename `claim_from_reclaim` → `claim_from_resume_grant` is a
straight find/replace for call sites. No known production consumer
relies on Valkey's `claim_from_reclaim` routing to
`ff_claim_resumed_execution` for the `lease_expired_reclaimable`
state (it returns `Ok(None)` on those inputs, indistinguishable
from "no grant available"). PG/SQLite consumers using the method
for suspend/resume continue to work under the renamed method.

CONSUMER_MIGRATION_0.12.md documents the rename with `cargo fix`
guidance (`cargo +nightly fix --broken-code` handles the
method-name change cleanly on most codebases).

---

## 7. Alternatives rejected

### 7.1 Cairn Option 1 — relax `ff_renew_lease` phase gate

Consumer drafts floated allowing `renew_lease` to succeed when the
execution is in a post-tool sub-phase but the caller still holds
the lease. Rejected: `mark_expired` clears `current_lease_id`, so
`renew_lease` fails on `stale_lease` before the phase gate fires.
The bug is not a gate-relax problem; it is a missing-recovery-path
problem.

### 7.2 Cairn Option 2 — `ff_describe_execution_phase` probe FCALL

Read-only FCALL returning `lifecycle_phase` +
`valid_operation_bitmask`. Diagnostic only — consumer learns
"reclaim not possible" but has no action. Does not unstick.
Rejected as insufficient; tracked separately as a future
observability addition.

### 7.3 Cairn Option 3 — `ff_complete_with_reclaim` atomic FCALL

Single FCALL that atomically reclaims + completes. Rejected as
over-scoped: ~150 LOC × 3 terminal variants (complete, fail,
cancel) + a new atomic invariant ("terminal write accepted
post-reclaim") with no precedent. The two-FCALL flow
(`ff_reclaim_execution` → `ff_complete_execution`) delivers the
same outcome under existing invariants.

**Two-FCALL timing window.** The reclaim issues a fresh lease at
the default TTL (30s+ in typical configurations); the operator
gap between `reclaim_execution` and `complete_execution` in
cairn's flow is sub-100ms (the two happen in-process after the
reclaim succeeds, before returning the HTTP response). The timing
window is orders-of-magnitude smaller than #371's original gap
class (30s+ operator waits). No `ff_complete_with_reclaim`
atomicity needed in practice; the RFC documents this assumption
so future scheduler changes that lengthen the gap re-evaluate.

### 7.4 Extend `ff_issue_claim_grant` input predicate

Reviewer C asked whether `ff_issue_claim_grant` could accept a
flag (`allow_reclaim: true`) to serve both admission paths from
one FCALL. Rejected: grant-issuance already gates on
`lifecycle_phase`, and reclaim consumption is semantically
distinct (new attempt + reclaim_count bump vs. fresh attempt).
Collapsing them conflates the admission predicates and forces the
consuming FCALL (`ff_claim_execution` vs. `ff_reclaim_execution`)
to branch on grant metadata — which is exactly the runtime-
dispatch shape Rev-2 rejected with the two-types decision (§3.1).

### 7.5 Merge `ClaimGrant` and new `ReclaimGrant` into one sum type

A single `enum Grant { Claim(...), Reclaim(...) }`. Rejected: the
fresh-claim path does not carry `lane_id` (the caller has it
separately); the reclaim path needs `lane_id` on the grant for the
FCALL's KEYS construction. A sum type with divergent
variants-shape is uglier than two types; consumers who want a
single match site at dispatch can wrap them in a crate-local enum.

### 7.6 Keep Rev-1 `GrantKind` discriminant on a single `ReclaimGrant`

Rev-1's shape. Rejected for Rev-2 per change #1 of the revision
changelog: the naming inversion (existing `ReclaimGrant` is
semantically a resume grant) would propagate forever; fixing it in
v0.13.0 is twice the migration pain and reviewer A/C consensus
was that the rename should happen in the same PR as the new type.

### 7.7 `max_reclaim_count = 100` default at Rust surface (Lua-parity)

Rev-1's default. Rejected — silent deadlock trap for pull-mode
consumers per §4.6.

### 7.8 Defer SQLite to v0.12.1

Rejected by owner decision #3 (revision changelog). All three
backends ship in v0.12.0.

---

## 8. Migration impact (consumer-facing)

### 8.1 Unchanged

- Every v0.11.0 behaviour for Valkey + Postgres + SQLite on
  surfaces not touched by RFC-024.
- `ff_issue_claim_grant` + `ff_claim_execution` +
  `ff_claim_resumed_execution` Lua semantics unchanged.
- The pre-RFC `claim_from_reclaim` semantic is preserved under the
  new name `claim_from_resume_grant` on all three backends.

### 8.2 Breaking at v0.12.0

1. **`ReclaimGrant` renamed to `ResumeGrant`.** Use-site rename in
   consumer code. `cargo fix` candidate. Type fields unchanged.
   `ReclaimToken` (`ff-core::backend::ReclaimToken`) renamed to
   `ResumeToken` for consistency.
2. **Trait method `claim_from_reclaim` renamed to
   `claim_from_resume_grant`** on `EngineBackend`. Every backend
   impl updates the method name. Consumers of the
   `FlowFabricWorker::claim_from_reclaim_grant` SDK method migrate
   to `claim_from_resume_grant` (pre-RFC the SDK method always
   dispatched to the resume path; the name matches the semantic
   post-rename).
3. **`ReclaimExecutionArgs.max_reclaim_count` type change**:
   `u32` → `Option<u32>`. Existing callers passing an explicit
   value wrap in `Some(...)`; callers wanting defaults pass
   `None`. Wire shape unchanged under `#[serde(default)]`.
4. **`IssueReclaimGrantArgs` gains `#[non_exhaustive]` +
   constructor.** Adds `worker_capabilities: BTreeSet<String>`
   field. No known production consumer (no pre-RFC Rust caller —
   test fixtures only).
5. **`ClaimGrant` + `ResumeGrant` + `ReclaimGrant` gain
   `#[non_exhaustive]`.** Struct-literal construction migrates to
   `::new()` constructors (all three types gain explicit
   constructors in this PR per `feedback_non_exhaustive_needs_constructor`).
6. **`HandleKind::Reclaimed` variant added.** Enum is already
   `#[non_exhaustive]`; additive but downstream exhaustive
   matches add an arm.
7. **`ReclaimExecutionOutcome` + `IssueReclaimGrantOutcome` are
   new enums, both `#[non_exhaustive]`** with explicit
   constructor pattern (no `::new()` needed — variants are the
   construction surface; consumers match, not construct).
8. **Migration `0015_claim_grant_table.sql` on PG + SQLite.**
   Applied on startup per existing migration runner. Backfills
   existing JSON-stashed claim grants; adds
   `lease_reclaim_count` column to `ff_exec_core`.

### 8.3 Additive (non-breaking)

1. **New trait methods** (`issue_reclaim_grant`,
   `reclaim_execution`) with default impls returning
   `Unavailable`. Pre-RFC out-of-tree backends keep compiling.
2. **New admin endpoint** `POST /v1/executions/{id}/reclaim` +
   matching SDK method. No change to existing endpoints.
3. **New `Supports::issue_reclaim_grant: bool` flag** on the
   RFC-018 capability matrix. Default `false`; in-tree backends
   set `true`.
4. **Lua `ff_reclaim_execution` gains `ARGV[9] =
   default_max_reclaim_count`.** Backwards-compatible — Lua reads
   with `or "100"` fallback so pre-RFC call sites (no known
   production callers) still parse.

### 8.4 Consumer migration path (cairn example)

1. `cargo update -p flowfabric` to v0.12.0.
2. `cargo fix` for `ReclaimGrant` → `ResumeGrant` +
   `claim_from_reclaim_grant` SDK call → `claim_from_resume_grant`
   where applicable.
3. Detect `capabilities().supports.issue_reclaim_grant == true` at
   connect time.
4. On `lease_expired` class from `POST /v1/runs/:id/complete`:
   call `admin.issue_reclaim_grant(req).await?` (new) instead of
   falling through to the existing `ff_issue_grant` claim-retry
   path.
5. On `Granted(grant)` response: call
   `worker.claim_from_reclaim_grant(grant).await?` (new shape).
6. On `NotReclaimable` / `ReclaimCapExceeded`: classify and
   terminal-log; do not retry.
7. Remove the existing `F64 bridge retry loop` at
   `cairn-rs/.../f64_bridge.rs` (marked `remove once FF#371 ships`
   per the consumer drafts).
8. `docs/CONSUMER_MIGRATION_0.12.md` §"RFC-024 reclaim wiring"
   documents this sequence.

---

## 9. Release readiness (hard gates)

All of the following must be satisfied before v0.12.0 ships.
RFC-024 ships joint with RFC-023 (SQLite).

- [ ] All three backends implement `issue_reclaim_grant` +
      `reclaim_execution` + `claim_from_resume_grant` (rename),
      and pass the RFC-018 capability-matrix snapshot test with
      `supports.issue_reclaim_grant = true`.
- [ ] Constructors landed in the same PR as each type gaining
      `#[non_exhaustive]`: `ClaimGrant::new`, `ResumeGrant::new`,
      `ReclaimGrant::new`, `IssueReclaimGrantArgs::new`,
      `ReclaimExecutionArgs::new` (per
      `feedback_non_exhaustive_needs_constructor`).
- [ ] Migrations `0015_claim_grant_table.sql` applied on PG +
      SQLite: fresh DB creates clean, existing DB with JSON-
      stashed grants backfills cleanly, `lease_reclaim_count`
      column populated with 0 on existing rows.
- [ ] Scheduler.rs (PG) read/write/delete paths flipped to
      `ff_claim_grant` table; three JSON sites (`scheduler.rs:35,
      252, 377`) updated; existing integration tests green.
- [ ] `ff-test` suite extended with a #371 deadlock-repro scenario
      on all three backends: pre-RFC deadlocks on Valkey +
      PG/SQLite divergent attempt_index; post-RFC all three
      recover via new endpoint with identical
      `current_attempt_index = N+1` + `lease_reclaim_count = 1`.
- [ ] Per-backend reclaim-cap test: reclaim counter hits the
      default (1000) and returns
      `ReclaimCapExceeded { reclaim_count: 1000 }` on all three
      backends; execution transitions to terminal_failed.
- [ ] Cairn-fabric integration test migrates from the `F64
      bridge retry` shape to the new `issue_reclaim_grant` shape;
      PR merged into cairn-rs tree.
- [ ] `docs/POSTGRES_PARITY_MATRIX.md` gains three rows
      (`issue_reclaim_grant`, `reclaim_execution`,
      `claim_from_resume_grant`) with all three backends marked
      `supported` at v0.12.0.
- [ ] `docs/CONSUMER_MIGRATION_0.12.md` §"RFC-024 reclaim
      wiring" section includes: (a) type renames (ReclaimGrant →
      ResumeGrant, new ReclaimGrant), (b) trait/SDK method
      renames, (c) `max_reclaim_count` Option<u32> migration,
      (d) new non_exhaustive constructors usage, (e) new
      HandleKind::Reclaimed arm, (f) `cargo fix` guidance.
- [ ] `CHANGELOG.md [Unreleased]`: `### Added` (new trait methods,
      new admin endpoint, new SDK method, new `ReclaimGrant`,
      new `HandleKind::Reclaimed`, new `Supports::issue_reclaim_grant`
      flag, migration 0015). `### Changed` (ReclaimGrant →
      ResumeGrant rename, claim_from_reclaim →
      claim_from_resume_grant rename, ReclaimExecutionArgs
      max_reclaim_count `u32` → `Option<u32>`, `#[non_exhaustive]`
      on grant types + args).
- [ ] `scripts/smoke-sqlite.sh` + SQLite scenario in
      `scripts/published-smoke.sh` extended with reclaim
      round-trip (issue grant → reclaim execution → complete on
      the fresh lease). Must pass before tag (per
      `feedback_smoke_before_release`).
- [ ] `scripts/smoke-v0.7.sh` or successor extended with the
      same reclaim round-trip against Valkey + Postgres.
- [ ] `examples/reclaim-pull-mode/` (~150 LOC per CLAUDE.md §5)
      demonstrating the cairn pull-mode recovery flow
      end-to-end. This is the new-example requirement for
      v0.12.0 headlines.
- [ ] RFC-018 capability snapshot JSON regenerated on all three
      in-tree backends.
- [ ] `release.yml` + `release.toml` + `RELEASING.md` — no new
      publishable crate introduced by RFC-024; publish-list
      unchanged. Confirmed in the release PR body.

PR partitioning (owner decision: multiple PRs allowed; all merge
before tag):

1. PR-A — RFC acceptance (doc-only).
2. PR-B — type renames (ReclaimGrant → ResumeGrant, ReclaimToken
   → ResumeToken); trait method rename; SDK method rename.
   No behaviour change; compile-break only.
3. PR-C — new grant types + non_exhaustive + constructors; new
   trait methods with default-Unavailable impls.
4. PR-D — Postgres migration 0015 + scheduler.rs table flip +
   new method impls.
5. PR-E — SQLite migration 0015 + new method impls.
6. PR-F — Valkey Lua ARGV[9] + new method impls + ReclaimExecutionArgs
   `max_reclaim_count: Option<u32>`.
7. PR-G — SDK admin endpoint + consumer example + docs + RFC-018
   capability flag + migration doc.
8. PR-H — Cairn-rs consumer-side migration (upstreamed to
   cairn-rs).

Smoke gate runs against the sum of PR-B through PR-G merged to
main before tag.

---

## 10. Maintenance tax commitment

| Item | Tax |
|------|-----|
| Two new trait methods on `EngineBackend` | Every new backend adds two impls; RFC-018 flags keep the cost machine-checkable. |
| One new admin HTTP route | Standard ff-server route-table addition; no new auth posture. |
| One new Lua ARGV on `ff_reclaim_execution` | Trivial; Lua already had the policy-override framework. |
| One new SQL table on PG + SQLite | Migration 0015; one follow-up (v0.13.0 migration 0017) to strip the JSON residue from `raw_fields.claim_grant`. |
| Two grant types replacing one | Constructor plumbing in each type; no ongoing cost after landing. |

Owner has accepted this tax per the four locked decisions at the
head. Cross-backend reclaim-semantic divergence (§6.1) retires
completely post-RFC-024 — no deferred convergence work.

---

## 11. Residual forks

None. All Rev-1 forks resolved by the four owner decisions
applied in Rev-2:

- §6.3 disposition of `claim_from_reclaim` — resolved to option
  (c) (rename in v0.12.0) per decision #1.
- `GrantKind::Default` — moot; `GrantKind` removed per decision
  reinforced by reviewers A-B6 + C framing.
- SQLite deferral — resolved to in-wave per decision #3.
- PR partitioning — resolved to multi-PR per decision #4.

Any new factual gap surfacing during implementation STOPS the
RFC acceptance + reports back; Rev-2 does not pre-authorize
further drift.

---

## 12. References

- Issue **#371** — tracking (pull-mode deadlock repro)
- **RFC-023** (SQLite dev-only backend, v0.12.0 wave) — release
  co-wave
- **RFC-020** (Postgres Wave 9, v0.11.0) — `claim_from_reclaim`
  reconciler precedent (renamed in RFC-024)
- **RFC-019** lease events — `reclaimed` event emitted by new
  path
- **RFC-018** capability discovery — new flag target
- **RFC-017** ff-server backend abstraction — trait surface
  precedent
- `crates/ff-script/src/flowfabric.lua:2985` —
  `ff_reclaim_execution` Lua primitive
- `crates/ff-script/src/flowfabric.lua:3036` — Lua
  `max_reclaim = 100` fallback
- `crates/ff-script/src/flowfabric.lua:3088` — Lua grant
  `worker_id` match (no worker_instance_id check)
- `crates/ff-script/src/flowfabric.lua:3585` — the
  `execution_not_eligible` gate that pull-mode currently hits
- `crates/ff-script/src/flowfabric.lua:3898` —
  `ff_issue_reclaim_grant`
- `crates/ff-backend-valkey/src/lib.rs:4323` — existing Valkey
  `claim_from_reclaim` (renames to `claim_from_resume_grant`)
- `crates/ff-backend-postgres/src/attempt.rs:286-396` — PG
  reconciler `claim_from_reclaim` (renames;
  `attempt_index`-preserving + epoch-bump semantics preserved
  under new name)
- `crates/ff-backend-postgres/src/scheduler.rs:35,252,377` — PG
  JSON-stashed `claim_grant` sites flipping to the new
  `ff_claim_grant` table
- `crates/ff-backend-sqlite/src/backend.rs:1328-1430` — SQLite
  Phase 2a.3 `claim_from_reclaim` (renames)
- `crates/ff-core/src/contracts/mod.rs:110,186` — `ClaimGrant`
  + `ReclaimGrant` (renaming to `ResumeGrant`) types
- `crates/ff-core/src/contracts/mod.rs:512` —
  `IssueReclaimGrantArgs` (extended)
- `crates/ff-core/src/contracts/mod.rs:540-570` —
  `ReclaimExecutionArgs` (`max_reclaim_count` flips to
  `Option<u32>`)
- `crates/ff-core/src/backend.rs:102` — `HandleKind` enum
  (gains `Reclaimed` variant)
- `crates/ff-sdk/src/admin.rs:125-201` — `claim_for_worker` SDK
  precedent for new endpoint shape
- Cairn-rs consumer drafts under `docs/design/ff-upstream/`
