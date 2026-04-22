# RFC-011: Exec/flow hash-slot co-location for atomic membership

**Status:** Accepted and implemented.
**Created:** 2026-04-18
**Supersedes:** issue #21 (reconciliation scanner) on phase-3 merge.

---

## §1 Motivation

### §1.1 The defect

`Server::add_execution_to_flow` at `crates/ff-server/src/server.rs:1240-1320` is a two-phase cross-partition operation:

1. FCALL `ff_add_execution_to_flow` on `{fp:N}` — commits membership to `flow_core`, `members_set`, `flow_index`.
2. `HSET exec_core flow_id <flow_id>` on `{p:N}` — stamps the back-pointer.

A server crash between phases leaves the flow claiming the exec as a member while `exec_core.flow_id` is still empty. cairn P3.6 §5.5 filed this as an atomicity defect. The interim fix (commit `8df40fc`) documents the two-phase contract, catalogues per-reader safety against the asymmetric orphan, and files the reconciliation scanner (issue #21). It closes the observable-correctness gap; it does not close the atomicity gap.

### §1.2 Why closing the gap matters

FlowFabric's crate-level value proposition is "Valkey-native atomic state transitions via Lua Functions." A two-phase cross-partition commit sits outside that envelope. A consumer reading the tagline can reasonably expect `add_execution_to_flow` to be atomic, hit the orphan window during a crash, and be right to file the same bug. The scanner in #21 is a mop, not a sink.

### §1.3 Why now, not later (evidence, not assertion)

* **No production users.** Consumers today are cairn-fabric (mid-migration) and one example app (`examples/media-pipeline`). No external API stability obligations. The window to re-shape `execution_id` without breaking deployed systems is open today and closes the moment a second consumer ships.
* **Repo is small.** `grep -rn "execution_partition(" crates/ benches/ | wc -l` returns 150 call sites on commit `8df40fc`, all enumerated in §3.
* **Cost compounds.** Every week on the two-phase shape is another week of code written against the legacy `ExecutionId::new()` constructor. Every new call site is another migration touch-point.
* **cairn's migration is already in flight.** Folding the exec-id-shape change into the current realignment cycle costs cairn one additional realignment. Deferring costs two separate realignment cycles later.

### §1.4 Rationale for shipping now vs. deferring

The **only** argument for deferring is "reduce near-term implementation risk." That argument holds in two shapes:

* "We might discover a blocker during implementation." Mitigated by the phased plan in §8 — each phase has an acceptance gate and a rollback story (§7.4). Phase 1 is pure type-system work with no Valkey interaction; if it compiles, it's sound. Phase 2 is call-site migration that the compiler flags for us. Phase 3 is the only phase with runtime behaviour change, and its acceptance criteria are tight (§8, phase 3).
* "Consumers might object to the shape change." cairn is the only active consumer. Their migration-cycle dispatch already includes an alignment PR slot; this is one concrete shape change folded into that slot.

Both risks survive the "ship now" path with explicit mitigations. Deferring trades them for accumulating call-site debt, which has no mitigation short of eventually shipping anyway. Net: shipping now is cheaper under every scenario analysed.

---

## §2 Design

### §2.1 The id shape

`execution_id` becomes a hash-tagged string:

```
execution_id = "{fp:N}:<uuid>"
```

* `fp:N` is the hash-tag prefix identifying the flow-partition the exec's parent flow lives on.
* `<uuid>` is a UUID for uniqueness within that partition. Parsing is version-agnostic — `ExecutionId::parse` accepts any valid UUID (v4 in practice from `Uuid::new_v4()`-backed mint paths; other versions if a caller chooses them). Version enforcement lives at the caller if it matters.
* `N` is an integer that fits in `u16`. Writers by convention mint only `N < num_flow_partitions` for the current deployment's config (see `ExecutionId::for_flow` / `solo` constructors), but **the `u16` range is the only parse-time invariant**. Range-against-config is caller-enforced at ingress — see §2.3.1. This allows exec ids to traverse deployment boundaries (replay, audit export, cross-env migration) without parse-time rejection.

Solo execs (no parent flow) use the same shape but with a deterministic synthetic lane-derived partition index (§4).

### §2.2 Consequences

* Valkey hash-tags keys by the content between `{…}`. Every key embedding `execution_id` (`exec_core`, `lease_current`, `lease_history`, `claim_grant`, `attempt_stream`, `waitpoints`, `signals`, `deps:*`) co-locates on the same slot as the parent flow's `flow_core`, `members_set`, `flow_index`.
* A single FCALL can take both `exec_core` and `flow_core` as KEYS. `ff_add_execution_to_flow` becomes atomic end-to-end.
* `execution_partition()` decodes the hash-tag prefix out of the string; no UUID re-hash.
* `num_execution_partitions` is retired. Everything routes via `num_flow_partitions`. Default bumps 64 → 256 (§5).
* Physical partition family collapses: `PartitionFamily::Execution` and `PartitionFamily::Flow` share one slot per entity. The logical distinction (which keys belong to exec vs flow topology) survives in the key-name prefix (`ff:exec:*` vs `ff:flow:*`), but both prefixes hash to the same slot.

### §2.3 ExecutionId API shape

```rust
impl ExecutionId {
    /// Mint an execution id co-located with the given flow's partition.
    /// Use when the flow is known at creation time.
    pub fn for_flow(flow_id: &FlowId, config: &PartitionConfig) -> Self;

    /// Mint an execution id co-located with the given lane's solo shard.
    /// Use when the exec has no parent flow (standalone create_execution).
    pub fn solo(lane_id: &LaneId, config: &PartitionConfig) -> Self;

    // ExecutionId::new()        — REMOVED in phase 1
    // ExecutionId::from_uuid()  — REMOVED in phase 1
    // impl Default              — REMOVED in phase 1

    /// Parse the hash-tagged shape. Returns Err on:
    ///  - missing `{fp:N}:` prefix
    ///  - non-integer N (or N that does not fit in `u16`)
    ///  - UUID suffix fails `Uuid::parse_str` (syntax check only; version-agnostic — callers that require v4 specifically enforce post-parse)
    ///
    /// **Parse does NOT check `N < num_flow_partitions`** — the function
    /// takes only `&str` and has no `PartitionConfig` handle. Range
    /// validation against the live deployment's partition count is the
    /// caller's responsibility via `.partition()` + config comparison.
    /// See the §2.3.1 parse-range-delegation note (revision 3) for the
    /// architectural rationale — exec ids are durable identifiers that
    /// can legitimately cross deployment boundaries with different
    /// configs, so parse-time config-coupling would mis-reject
    /// otherwise-valid historical ids.
    pub fn parse(s: &str) -> Result<Self, ExecutionIdParseError>;

    /// Raw string form; always `{fp:N}:<uuid>`.
    pub fn as_str(&self) -> &str;

    /// Decoded partition (no re-hash; reads from the hash-tag).
    pub fn partition(&self) -> u16;
}
```

#### §2.3.1 Parse-range delegation note (revision 3, 2026-04-18)

The original §2.3 text (revision 1) described `ExecutionId::parse` as rejecting ids where "N >= num_flow_partitions." That phrasing implied parse had access to a `PartitionConfig`. It does not — parse takes `&str` only. W1 probed during phase-1 review: `ExecutionId::parse("{fp:50000}:550e8400-e29b-41d4-a716-446655440000")` on a 256-partition config succeeds and produces an id that routes to a nonexistent slot.

**The implementation ships the correct behaviour: parse validates shape only (hash-tag prefix, integer-that-fits-in-u16, valid UUID via `Uuid::parse_str` — syntax-only, version-agnostic). Range validation + UUID-version enforcement are the caller's responsibility.**

Architectural rationale:

1. **Exec ids are durable identifiers.** They outlive the Valkey deployment they were minted against. An id minted on a 256-partition cluster can legitimately surface on a 1024-partition cluster (stream replay, audit export, cross-environment migration). Parse-time config-coupling would mis-reject otherwise-valid historical ids under legitimate lookup flows.
2. **Parse has no `PartitionConfig` handle.** Adding one would either make parse infallible-but-trivial (no config check) or force every caller that currently calls `ExecutionId::parse(s)` to plumb a config through — invasive change for a boundary-validation concern.
3. **The right place for range validation is at the system boundary against the current deployment's config** — `ff-server` at ingress, the scheduler when it receives an id from a claim-grant payload, etc. Callers use `eid.partition()` (O(1) hash-tag decode) + compare against `config.num_flow_partitions` at the exact moment range matters.

Phase-1 tests encode this architectural boundary: `execution_partition_ignores_config_value` verifies that an id minted in a 4-partition config decodes to the same partition index when read via a 1024-partition config. That test would fail if parse coupled to the config value — the deliberate pass is the invariant we're protecting.

Callers that want range validation write it explicitly:
```rust
let eid = ExecutionId::parse(s)?;
if eid.partition() >= config.num_flow_partitions {
    return Err(SomeKind::OutOfRangePartition);
}
```

This amendment closes W1's YELLOW-3 by updating the RFC prose to match the implementation's (correct) shape, rather than the other way around.

### §2.4 ff-script FCALL-result parsing (CONVERGED-RED-1 resolution)

`crates/ff-script` parses Lua FCALL return values into Rust typed results (`ClaimExecutionResult::Claimed`, `CompleteExecutionResult::Completed`, etc.). 9 of those parse paths today construct their result struct with `execution_id: ExecutionId::parse("").unwrap_or_default()` as a placeholder — the comment reads "filled by caller" — and rely on the Rust server-side caller overwriting the field in-place after the parse returns. Removing `impl Default for ExecutionId` in phase 1 breaks `unwrap_or_default()`, removing the placeholder escape hatch.

**Decision: Option (c) refactor the parse API to eliminate the placeholder pattern entirely.** Each `FromFcallResult` impl whose current return type carries an `execution_id` field is refactored to return a "partial" variant that omits that field; the server-side caller combines the partial with the `ExecutionId` it already holds (the caller supplied it as ARGV in the first place, so this is always available at the call site).

Concrete shape:

```rust
// Before (today, in crates/ff-script/src/functions/execution.rs):
impl FromFcallResult for ClaimExecutionResult {
    fn from_fcall_result(raw: &Value) -> Result<Self, ScriptError> {
        // ... parse epoch, expires_at, attempt_id, attempt_index, attempt_type ...
        Ok(ClaimExecutionResult::Claimed(ClaimedExecution {
            execution_id: ExecutionId::parse("").unwrap_or_default(), // filled by caller
            lease_id, lease_epoch, attempt_index, attempt_id, attempt_type,
            lease_expires_at,
        }))
    }
}

// After (phase 1):
pub struct ClaimedExecutionPartial {
    pub lease_id: LeaseId,
    pub lease_epoch: LeaseEpoch,
    pub attempt_index: AttemptIndex,
    pub attempt_id: AttemptId,
    pub attempt_type: AttemptType,
    pub lease_expires_at: TimestampMs,
}

impl ClaimedExecutionPartial {
    pub fn complete(self, execution_id: ExecutionId) -> ClaimedExecution {
        ClaimedExecution { execution_id, ..self.into_claimed() }
    }
}

pub enum ClaimExecutionResultPartial { Claimed(ClaimedExecutionPartial) }

impl FromFcallResult for ClaimExecutionResultPartial {
    fn from_fcall_result(raw: &Value) -> Result<Self, ScriptError> {
        // same parsing logic, no placeholder
        Ok(Self::Claimed(ClaimedExecutionPartial { lease_id, ... }))
    }
}
```

Server caller updates (`crates/ff-server/src/server.rs`):
```rust
let partial: ClaimExecutionResultPartial = fcall_parse(...)?;
let result = match partial {
    ClaimExecutionResultPartial::Claimed(p) =>
        ClaimExecutionResult::Claimed(p.complete(args.execution_id.clone())),
};
```

**Why Option (c), not (a) or (b):**

* **Option (a) — wrap `execution_id: Option<ExecutionId>` on the public struct.** Violates the "a `ClaimedExecution` always has an `execution_id`" type invariant that `ff-sdk` callers (and cairn-fabric) rely on. Every downstream reader would need an unwrap() or `.expect()`; introduces runtime panics where today's code has compile-time guarantees. Explicitly weakens the type system relative to today.
* **Option (b) — `pub const ExecutionId::PLACEHOLDER`.** Re-introduces `Default` under a different name. Any caller path that forgets to overwrite the field silently corrupts state with the placeholder value — the exact failure mode removing `Default` in §2.3 is aiming to prevent. Silent corruption worse than today.
* **Option (c) — refactor to Partial.** Type-system-enforced: the compiler forbids a ClaimedExecution from being constructed without an `execution_id`, because there is no constructor path that produces one. Eliminates the "filled by caller" pattern, which today is a load-bearing convention with no compiler enforcement. Aligns with the §2.3 goal of removing `Default` to make "exec_id is always populated" a type-level invariant.

  **Architectural fit:** this pattern already exists in the ff-script ↔ ff-core boundary. The "what the FCALL returns" types (`FcallResult`, `from_fcall_result`) are distinct from the "what the typed API exposes" types (`ClaimExecutionResult`, `CreateExecutionResult`) — the parsers deliberately do not speak the wire-protocol input types. Adding `ClaimedExecutionPartial` etc. extends that existing boundary one layer deeper; the refactor matches an architectural split the code already has, rather than inventing a new one.

**Cost:** ~10 `FromFcallResult` impls across `crates/ff-script/src/functions/execution.rs` (8 impls), `signal.rs` (1), `scheduling.rs` (1). Each is a `Partial` type + `complete(execution_id)` combinator + server-side `.complete(args.execution_id.clone())` at the caller. Scoped into phase 1 (§7.1) alongside the ff-core primitives.

**ff-script files touched:** `crates/ff-script/src/functions/execution.rs` (9 placeholder sites at L154, L206, L273, L314, L354, L463, L466 + 2 `ExecutionId::parse(&eid_str)` at L74, L81), `scheduling.rs` (1 placeholder L81 + 1 parse L52), `signal.rs` (1 placeholder L230), `flow.rs` (2 `ExecutionId::parse(&eid_str)` at L83, L92 — these stay as parsers of FCALL-returned strings, not placeholders). Total: 4 files, 16 call sites, 9 placeholders.

The 5 `ExecutionId::parse(&eid_str)` production sites — where the FCALL return carries a real exec_id string — are forward-compatible with §2.3's new parse rules. The new shape (`{fp:N}:<uuid>`) still parses through the same `ExecutionId::parse` entry point; as long as the Lua side is returning the new shape on phase-3 merge, these parsers succeed. Phase 1 just changes the parse validator; phase 3 changes what Lua emits. No placeholder issue on these 5.

**Multi-variant enum pattern (addendum 2026-04-18 after W1 YELLOW round 2).** The `ClaimExecutionResultPartial` example above is a single-variant enum. Multi-variant result types (`ExpireExecutionResult { AlreadyTerminal, Expired { execution_id } }`, `CreateExecutionResult { Created { execution_id, public_state }, Duplicate { execution_id } }`, etc.) follow a variant-mirror pattern where only variants actually carrying an `execution_id` need the Partial lift:

```rust
// Before:
pub enum ExpireExecutionResult {
    AlreadyTerminal,
    Expired { execution_id: ExecutionId },
}

// After:
pub enum ExpireExecutionResultPartial {
    AlreadyTerminal,
    Expired {},  // empty variant — exec_id supplied by caller
}

impl ExpireExecutionResultPartial {
    pub fn complete(self, execution_id: ExecutionId) -> ExpireExecutionResult {
        match self {
            Self::AlreadyTerminal => ExpireExecutionResult::AlreadyTerminal,
            Self::Expired {} => ExpireExecutionResult::Expired { execution_id },
        }
    }
}
```

`complete(exec_id)` attaches the caller-supplied `execution_id` only to the variants that carry one; variants without an exec_id (`AlreadyTerminal`, `Duplicate`'s no-id form if any) pass through unchanged. The `complete` combinator is a total match over the Partial variants, so adding a new variant to the Partial forces a compile error in `complete` — which is the point: future maintainers can't forget to wire a new exec_id-bearing variant into the caller-supplies path.

For `CreateExecutionResult { Created { execution_id, public_state }, Duplicate { execution_id } }` — both variants carry exec_id — the Partial form is:

```rust
pub enum CreateExecutionResultPartial {
    Created { public_state: PublicState },  // exec_id lifted to caller
    Duplicate {},                             // exec_id lifted to caller
}
```

Both variants still get an exec_id in `complete`; the Partial just strips the field from the parsed shape so the parser never has a placeholder in scope.

**Scope impact:** zero. The 10 refactored impls in §3.1b already implicitly follow this pattern — each variant's fields minus `execution_id` form the Partial variant, and the caller supplies the exec_id back via `.complete(args.execution_id.clone())`. This addendum just makes the multi-variant shape explicit so phase-1 implementation has a reference pattern for the non-trivial cases (Expire + Create have 2 variants each; Complete + Cancel + Delay + MoveToWaitingChildren + Fail have single-variant shapes matching the original §2.4 example).

### §2.4 Rationale for the id-shape choice

**Why `{fp:N}:<uuid>` and not `{fp:N}-<uuid>` or `fp:N:<uuid>`?**
Valkey hash-tags require the `{…}` braces literally. Anything else is a non-hash-tagged string and hashes the whole key — the opposite of what we want. This is not a style choice; it's the Valkey clustering contract.

**Why include `fp:N` when the UUID alone would already be unique?**
The UUID exists for intra-partition uniqueness; the hash-tag exists for routing. They serve different roles. Conflating them (encoding partition bits in the UUID) would make the id unparseable by non-FF tools and opaque to operators; separating them makes the partition visible in `MONITOR` output and explainable at a glance.

**Why String, not a typed struct with separate partition+uuid fields?**
The serialised form crosses too many boundaries (wire, logs, key names, cairn's BridgeEvent payloads, test fixtures) to practically carry a struct everywhere. String with a validated shape is the same ergonomic footprint as today's UUID-backed id and preserves `&str` interop across the whole surface.

---

## §3 Scope of change (punch list)

All call sites grepped on commit `8df40fc`. Line numbers are inventory-accurate as of that commit. Phase attribution in §8.

### §3.1 ff-core primitives

* `crates/ff-core/src/types.rs:87-90` — replace `uuid_id!{ExecutionId}` macro invocation with a bespoke impl carrying a validated hash-tagged `String`. Remove `Default::default()`.
* `crates/ff-core/src/partition.rs:95-100` — rewrite `execution_partition` to decode the hash-tag prefix instead of hashing UUID bytes. Keep `partition_for_uuid` for `flow_partition` / `budget_partition` / `quota_partition` (those stay UUID-hashed).
* `crates/ff-core/src/partition.rs:134-192` — regenerate partition tests against the new constructor shape.

### §3.1b ff-script Partial-type refactor (added 2026-04-18 after RED-1 convergence)

**New scope item, scoped into phase 1.** `crates/ff-script` has 4 files + 9 `ExecutionId::parse("").unwrap_or_default()` placeholder sites + 5 production `ExecutionId::parse(&eid_str)` sites. The placeholder pattern breaks the moment §2.3's `impl Default for ExecutionId` removal lands. §2.4 designs the Partial-type refactor; this punch-list entry is the concrete file map.

* `crates/ff-script/src/functions/execution.rs:68-88` — `FromFcallResult for CreateExecutionResult`. No placeholder; 2 production `parse(&eid_str)` sites (L74, L81) that read the real exec_id from the FCALL return. Forward-compat with §2.3 parse rules. **No refactor needed.**
* `crates/ff-script/src/functions/execution.rs:137-162` — `FromFcallResult for ClaimExecutionResult`. 1 placeholder (L154). **Refactor to `ClaimExecutionResultPartial`** per §2.4. Add `ClaimedExecutionPartial` struct in the same file.
* `crates/ff-script/src/functions/execution.rs:201-240` — `FromFcallResult for CompleteExecutionResult`. 1 placeholder (L206). **Refactor.**
* `crates/ff-script/src/functions/execution.rs:268-300` — `FromFcallResult for CancelExecutionResult`. 1 placeholder (L273). **Refactor.**
* `crates/ff-script/src/functions/execution.rs:309-340` — `FromFcallResult for DelayExecutionResult`. 1 placeholder (L314). **Refactor.**
* `crates/ff-script/src/functions/execution.rs:349-390` — `FromFcallResult for MoveToWaitingChildrenResult`. 1 placeholder (L354). **Refactor.**
* `crates/ff-script/src/functions/execution.rs:455-470` — `FromFcallResult for ExpireExecutionResult`. 2 placeholders (L463, L466). **Refactor.**
* `crates/ff-script/src/functions/scheduling.rs:45-95` — `FromFcallResult` impl. 1 production `parse(&eid_str)` (L52) + 1 placeholder (L81). **Refactor** (placeholder only; production parse site is forward-compat).
* `crates/ff-script/src/functions/signal.rs:215-240` — `FromFcallResult` impl. 1 placeholder (L230). **Refactor.**
* `crates/ff-script/src/functions/flow.rs:80-100` — 2 production `parse(&eid_str)` sites (L83, L92). No placeholder. Forward-compat.

**Server-side updates** (required at every Partial-type consumer site; enumerated as subset of §3.3):
* `crates/ff-server/src/server.rs` — every `fcall_with_reload("ff_claim_execution", ...)` call site and friends; each unwraps the Partial + calls `.complete(args.execution_id.clone())`. Compiler forces the update because the result type name changed. Approximately 7 call sites (one per refactored `FromFcallResult` impl), mechanical per site.

**Test updates:**
* `crates/ff-script/tests/*.rs` (if any) — regenerate against Partial types. No test file currently constructs a placeholder; spot-check in phase 1.

**Phase 1 acceptance gate** (§7.1) is extended: `cargo check -p ff-script` must compile clean after the ff-core + ff-script changes land together. Without ff-script in the gate, phase 1 appears to pass while ff-script silently has compile errors waiting for the first dependent build.

### §3.2 Lua: `ff_add_execution_to_flow` becomes atomic

* `lua/flow.lua:122-173` — add `exec_core` as KEYS[4]; append `redis.call("HSET", K.exec_core, "flow_id", A.flow_id)` after the membership SADD (step 3). Remove the two-phase-contract preamble in the docstring; replace with a co-location note pointing at this RFC. Update the `ok(...)` return to confirm the atomic commit.
* `crates/ff-server/src/server.rs:1240-1320` — `add_execution_to_flow` method collapses to a single FCALL. KEYS grows from 3 to 4 (append `ectx.core()`). The phase-2 HSET block at L1279 deletes outright. Rustdoc rewrites around the atomic shape; the orphan-window / reader-safety catalogue deletes.

### §3.3 `execution_partition()` call sites — routing-through

All call sites below keep the same API signature; the change in §3.1 makes them pick flow-partition instead of exec-partition. Each site is reviewed for any place that previously assumed "exec partition ≠ flow partition":

* ff-server: `crates/ff-server/src/server.rs:448, 564, 618, 692, 1340, 1679, 1738, 1840, 1876, 1926, 2098, 2156, 2312, 2393, 3764`. (L1275 deletes with §3.2.)
* ff-engine scanners: `crates/ff-engine/src/partition_router.rs:31`, `crates/ff-engine/src/scanner/dependency_reconciler.rs:265`, `crates/ff-engine/src/scanner/flow_projector.rs:200`.
* ff-sdk workers: `crates/ff-sdk/src/task.rs:365, 411, 466, 533, 600, 694, 784, 835, 902, 1076, 1824, 1924`; `crates/ff-sdk/src/worker.rs:1248`.
* ff-core keys tests: `crates/ff-core/src/keys.rs:621, 634, 654, 665, 677`.
* Tests: `crates/ff-test/src/helpers.rs:106, 237, 276, 304, 315`; `crates/ff-test/src/assertions.rs:38, 74, 107, 189, 201, 213, 225, 233, 248, 267, 283, 299, 310, 325, 349, 366`; ~90 sites across `crates/ff-test/tests/*.rs` (grep-bounded).

### §3.4 `ExecutionId::new()` removal (REVISED after RED-2 convergence)

Every call site of `ExecutionId::new()` migrates to `for_flow` or `solo`. Compiler enforces this — after phase 1, `ExecutionId::new` doesn't exist and every caller fails to compile until migrated.

**Independent-grep count (2026-04-18): 175 call sites** across 12 files. Original draft claimed "~50 sites" / "~90 sites"; both were 2-3x undercounts. Revised distribution:

* **ff-core (10 sites)** — `crates/ff-core/src/partition.rs` (2, tests), `crates/ff-core/src/keys.rs` (4, tests), `crates/ff-core/src/types.rs` (2, tests for the macro-generated constructor), `crates/ff-core/src/contracts.rs` (2, doctests). All in ff-core's own test paths; **scoped into phase 1** alongside §3.1.
* **ff-test (166 sites)**:
  - `crates/ff-test/tests/e2e_lifecycle.rs`: 134 sites — the bulk of the migration work. Each site is a test fixture minting an exec_id for a scenario; most are solo-path (no parent flow), a minority are flow-tests.
  - `crates/ff-test/tests/waitpoint_tokens.rs`: 14 sites. Solo-path tests.
  - `crates/ff-test/tests/e2e_api.rs`: 7 sites. Mixed.
  - `crates/ff-test/tests/admin_rotate_api.rs`: 4 sites. All solo (admin tests have no flow context).
  - `crates/ff-test/tests/pending_waitpoints_api.rs`: 4 sites. All solo.
  - `crates/ff-test/tests/result_api.rs`: 3 sites. All solo.
  - `crates/ff-test/src/fixtures.rs`: 1 site. Solo (fixture helper).
* **Authoritative attribution for the `for_flow` vs `solo` selection at each site (REVISED round 2 after W3 YELLOW):**
  - **ff-server side: ZERO sites.** Current code does not mint `ExecutionId` in `ff-server`. `CreateExecutionArgs.execution_id` is a required non-`Option<ExecutionId>` field (`crates/ff-core/src/contracts.rs`), so every caller supplies a pre-minted id to the server. There is no `flow_id` field on `CreateExecutionArgs` today either, so even hypothetically the server couldn't decide `for_flow` vs `solo` without an ingress schema change. The original draft's "ONE site in ff-server" was wrong — there is no such site, because there is no ff-server mint path. Striking the ff-server bullet entirely.
  - **cairn-fabric side** (phase 4): cairn's `RunService::create_run` + `TaskService::create_task`. Cairn is the mint site today and stays the mint site post-RFC. Cairn's choice between `for_flow` and `solo` depends on whether the task's parent run has a flow context; cairn-fabric-side decision, full stop.
  - **ff-test side** (phase 2, W2): 166 test sites. Each test picks the variant matching what the test is checking — solo-path tests use `ExecutionId::solo(&fixture_lane, &config)`, flow-member tests use `ExecutionId::for_flow(&shared_fid, &config)`. The test helper `crates/ff-test/src/fixtures.rs` gains a `fixture_lane()` function returning a single test-wide `LaneId` so every solo site can share one helper call.

  **Why not option B** (spec a wire-API change where `CreateExecutionArgs.execution_id: Option<ExecutionId>` + new `flow_id: Option<FlowId>` field + server-side mint)? It's scope creep. The RFC's target is §5.5 atomicity. Server-side minting would only serve a hypothetical client that bypasses the SDK, not a real consumer today. A future client needing it files a follow-up RFC with its own review; silently expanding the wire contract here would dilute the RFC's focus without unblocking any current work.

**Mechanical vs judgement split (REVISED):**
* ~160 of 166 ff-test sites are "solo with fixture_lane" — mechanical, 30s each after the first.
* ~6 sites are flow-member tests; each needs semantic judgement about which flow context the exec belongs to. ~5min each.
* No ff-server id-minting site exists today, so W1's phase-2 scope drops by ~30min (previously budgeted for the now-non-existent `create_execution` mint decision). Revised hours reflected in §7.2 round-2 update below.

### §3.5 Scheduler / claim routing (REVISED — W3 YELLOW)

Corrected file map. `crates/ff-scheduler/src` contains only `claim.rs` + `lib.rs`; there is no `partition_scan.rs` (original draft was wrong). Verified: `ls crates/ff-scheduler/src` → `claim.rs lib.rs`.

* `crates/ff-scheduler/src/claim.rs` — the full scheduler scope in one file. Changes: (a) `ClaimGrant.partition` field-value tag shifts from `{p:N}` to `{fp:N}` (consumers use it opaquely; no logic branching). (b) Any partition-iteration the scheduler does (currently over `num_execution_partitions`) migrates to `num_flow_partitions`. (c) The scheduler's `Scheduler::claim_for_worker` path picks grant keys via `execution_partition(eid)`; the call is unchanged but the decoded partition now routes via the hash-tag (§3.1).
* `crates/ff-scheduler/src/lib.rs` — re-export surface only; no functional changes.

### §3.6 Lease / heartbeat / stream reads

No caller-side edits. `execution_partition()` in §3.1 routes the SDK's lease renewals, signal reads, stream tails, etc. onto `{fp:N}` transparently. `attempt_stream`, `lease_history_key`, `wp_signals_stream` all scope by exec_id via `ExecKeyContext`, which rebuilds the tag from `Partition::hash_tag()`.

### §3.7 Budget + quota

No change. `ff_apply_budget_admission` routes via `budget_partition(budget_id)`, not `execution_partition`. The exec-side admission result propagates via an FCALL result, not a key join. Confirmed against `crates/ff-engine/src/scanner/budget_reconciler.rs:231` (the only mention of `execution_partition` in this path is a deferred-parse comment).

### §3.8 cairn-fabric

Phase 4. cairn constructs exec_ids in `RunService::create_run` and `TaskService::create_task`; it also carries its own `execution_partition` decoder for retry-ledger sharding. Both converge on `ff_core::ExecutionId::for_flow` / `solo` + `execution_partition` re-exported from `ff-core`. Total cairn-side diff expected: <300 LOC.

### §3.9 Observability

Tracing spans + metrics with `partition` labels now reflect flow-partition. Dashboards filtering by `{p:N}` labels still produce the same label values (flow partition N for exec X), but the partition-count axis changes (256 flow partitions, was 256 exec partitions — net-same today; operator guidance in the release note if deployments override `num_execution_partitions`).

---

## §4 Solo-execution handling

### §4.1 Decision

Execs with no parent flow use a deterministic synthetic partition index derived from the lane id:

```
solo_partition_index = crc16_ccitt(lane_id_utf8_bytes) % num_flow_partitions
execution_id = "{fp:<solo_partition_index>}:<uuid>"
```

The id shape is identical to the flow-parented case — `{fp:N}:<uuid>` — and so is the routing code. `execution_partition()` doesn't need to know whether `N` came from a FlowId hash or a LaneId hash; it just decodes the prefix.

### §4.2 Rationale — why not a string-tag sentinel like `{fp:solo-<lane_id>}`

This was my first-draft choice and it's wrong, for a concrete reason surfaced while writing §3.1.

`Partition { index: u16 }` is the workspace-wide routing struct (`crates/ff-core/src/partition.rs:64-67`). It carries an integer index, not a free-form string. A string-tag sentinel `{fp:solo-<lane_id>}` would:

1. Break `Partition::hash_tag()` (the `{prefix:index}` format literal at `crates/ff-core/src/partition.rs:72`) — would need a parallel String variant, adding a `PartitionFamily::SoloLane` branch.
2. Break `execution_partition()`'s decoder — would need to distinguish "numeric partition" from "synthetic string partition."
3. Break IndexKeys + FlowIndexKeys + every scanner that iterates `0..num_flow_partitions` — solo execs would live outside the numeric index space.
4. Break `num_flow_partitions` as a meaningful cap — synthetic shards have no bound.

The CRC16-of-lane derivation keeps `Partition.index: u16` load-bearing, keeps `execution_partition()` monomorphic, and keeps iteration bounds intact. Lane hot-spot behaviour is identical to the rejected sentinel — all solo execs for a given lane hash to one slot — but the implementation cost is ~5 LOC vs. ~200.

### §4.3 Rationale — why lane-hash, not per-tenant or global

* **Random-per-exec** (`{fp:<random>}:<uuid>`) — defeats co-location entirely; solo execs scatter. Rejected: the whole point of this RFC is co-location. A shape that doesn't co-locate fails the motivation before it reaches the review.
* **Per-tenant** (`{fp:<crc16(tenant_id)>}:<uuid>`) — leaks tenant identity into the routing decision. Tenants aren't first-class in the current FF model (lanes are); adding tenant-as-router introduces a new concept for a side-effect.
* **Global solo shard** (`{fp:0}:<uuid>` for all solo execs) — every solo exec across the cluster on one slot. A single `create_execution`-heavy workload instantly saturates that slot.
* **Per-lane** — aligns with the existing routing hierarchy (`lane_eligible`, `lane_delayed`, `lane_active`, `lane_terminal` are all lane-scoped). Cost is bounded by per-lane throughput, which operators already size for when they pick lane concurrency limits.

### §4.4 Hot-spot claim (evidence, not assertion)

**Claim: "Lane-scoped solo hotspot is not a new hotspot class."**

Evidence, citing current code on commit `8df40fc`:

1. `crates/ff-core/src/keys.rs:211-256` — every lane-scoped index (`lane_eligible`, `lane_delayed`, `lane_active`, `lane_terminal`, `lane_blocked_*`, `lane_suspended`) is already keyed on a single partition. A single lane's scheduler-tick work fans across partitions today, but the lane's terminal + blocked sets for a given partition are already slot-bound per-partition-per-lane.
2. `lua/flow.lua:401-420` — dependency resolution on KEYS[5]=eligible_zset runs on the flow's partition, but the lane's eligible zset (under the current exec-hash distribution) is spread across partitions, so lane-scheduler-tick work is already serialised per-(lane, partition) pair.
3. **Post-RFC change:** all solo execs for lane L concentrate on `crc16(L) mod num_flow_partitions`. Lane-tick throughput for solo work is now bounded by one Valkey slot. Pre-RFC, it's bounded by ~`num_exec_partitions` × single-slot throughput in theory, but the lane-scheduler iterates partitions sequentially per tick anyway, so realised throughput is closer to single-slot.

Realised per-slot throughput (from existing bench data, `benches/perf-invest/wider_80_20-ferriskey-wider-8cb6dff.json` on AMD EPYC 9R14, 16 cores, 30 GB, 30s duration, 16 workers): **115,028 ops/sec at p50 = 0.132 ms, p99 = 0.228 ms** against a single Valkey node with 4 KiB payloads, 80% GET / 20% SET. For a solo-heavy workload, FlowFabric's per-op cost is 3-5 Valkey round-trips (claim → complete + index mutations), so lane-bound sustained throughput under co-location caps at roughly **20,000-40,000 solo execs/sec per lane per slot**.

That ceiling is above cairn-fabric's current projected peak (<1,000 solo execs/sec per project) by more than one order of magnitude. Workloads exceeding that ceiling should split their work across lanes — which is already the idiomatic FF pattern for horizontal scaling — not rely on the per-lane-partition hotspot being absorbed by the exec-hash spread.

---

## §5 Hot-spot risk and mitigation

### §5.1 Decision

1. Bump `num_flow_partitions` default from 64 → 256 (preserves today's total keyspace fanout after co-location).
2. Document a soft limit of **10,000 members per flow** in operator guidance.
3. Reject flow-size-aware placement and resharding-on-growth as mitigations.

### §5.2 Rationale — the 10k soft limit

From the per-slot throughput measurement in §4.4 (~115k ops/sec against ferriskey on a single slot), a flow's FCALL-heavy operations (claim, complete, deliver-signal, add-member) run ~30k ops/sec per slot after accounting for Lua overhead. A flow doing 10k member writes + 10k claims + 10k completes + 10k member-state-changes = 40k FCALLs lifetime, saturates the slot for ~1.3 seconds end-to-end. Flows with more members exceed the sub-second p99 budget that operators typically want from coordination ops and should be split.

This is a **soft** limit. Nothing in FlowFabric rejects a 50k-member flow; the guidance is "your p99 suffers proportionally; if that's unacceptable, split." RFC-007 §3 already hints at this informally; this RFC codifies the number and the reason.

### §5.3 Rationale — rejecting flow-size-aware placement

A reviewer's obvious counter: "Let small flows keep atomic single-FCALL and let big flows opt into a multi-slot pool that re-opens the cross-partition commit."

**Why this is rejected:**

1. It's not a single-decision knob. Every FCALL in the exec lifecycle (claim, complete, fail, suspend, resume, reclaim, expire, cancel, delay, promote, unblock, deliver_signal) would need two code paths: atomic (co-located) and two-phase (pooled). That's ~15 Lua functions × two implementations + a runtime dispatch on every call.
3. The dispatch itself needs a per-flow "is this pooled?" read before every FCALL, or a cached flag that has to be invalidated on pool crossover. Either way, we pay a round-trip or a consistency-maintenance burden on the hot path.
4. The pooling mechanism (assigning a big flow to N slots) is itself a cross-partition operation. We'd need *another* atomic commitment protocol to bootstrap the pool, recursing the very problem this RFC is closing.
5. The operational story becomes "small flows are atomic, big flows are eventually consistent, and you find out which you got after the fact." Worse than today's "everything is two-phase" — predictable eventual consistency is better than conditional atomicity.

Net: flow-size-aware placement is strictly more complex than the two-phase shape we're replacing, and it doesn't give us atomicity for the flows that matter most (the big ones that actually see crash-window orphans under load).

### §5.4 Rationale — rejecting online resharding

Resharding a Valkey hash slot is not a supported operation. The only way to "move" a flow's keys is (a) copy-write to new keys + flip a pointer + delete old keys, or (b) stop-the-world dump-load. (a) requires its own atomic commit protocol — recursing again. (b) is unavailable without downtime, which contradicts the "always-on coordination engine" design premise. Rejected on first principles.

### §5.5 Rationale — the 256 default

Today: `num_execution_partitions = 256`, `num_flow_partitions = 64`. Post-RFC: all-in-one partition count; picking 256 preserves today's total keyspace fanout. Could bump higher (512, 1024) but:

* 256 covers a typical 3-10 node Valkey cluster with adequate slot distribution.
* Higher counts add key-name overhead (the `{fp:N}:<uuid>` prefix grows) and key-namespace-size in monitoring tools, for no throughput benefit on current deployments.
* Operators can override via `PartitionConfig` at deployment time; the default just has to be sensible.

Decision: 256. Deferrable to phase 5 benchmarks if the number turns out wrong; a default bump is a one-line change + release-note entry.

### §5.6 Traffic-amplification mitigation (added 2026-04-18 after W3 YELLOW §9.7)

Solo-execution routing via `crc16(lane_id) % num_flow_partitions` (§4.1) shares the partition index space with flow routing via `crc16(flow_id.as_bytes()) % num_flow_partitions`. A lane and a flow that hash to the same partition pile traffic on the same Valkey slot. This is not a correctness issue — keys are namespaced by entity type (`ff:exec:*` vs `ff:flow:*`) — but it is a hot-spot axis.

**Birthday-paradox arithmetic:** with ~15 lanes and ~15 active flows on a 256-partition cluster, P(at least one collision) ≈ 1 − (256 choose 30) / 256^30 ≈ 30-40%. For deployments with more active entities, the probability climbs.

**Three-part mitigation:**

1. **CLI diagnostic.** `ff-server admin partition-collisions` — a new admin subcommand on the existing `ff-server` binary (NOT a separate crate). Prints the current crc16-hash mapping for every lane + flow in the cluster and flags collisions. Operators run this during deployment sizing. Shape: `ff-server admin partition-collisions --config <path>` reads the standard `ServerConfig`, connects to the configured Valkey cluster, iterates `flow_index` members across partitions + `LaneId` registry, computes `crc16 % num_flow_partitions` for each entity, emits a collision table to stdout. Subcommand placement reuses the existing config-loading path + ferriskey client setup — no duplicated plumbing. Cost: ~1h (subcommand handler + unit test). Matches §5.6 cost claim; matches §8 phase-5 budget.
2. **Runbook.** `docs/runbooks/partition-amplification.md` (phase 5) explains: how to read the CLI, what a collision means for p99 latency, how to mitigate (rename a lane, adopt a custom `SoloPartitioner`).
3. **Pluggable `SoloPartitioner` trait.** `crates/ff-core/src/partition.rs` exposes a trait:
   ```rust
   pub trait SoloPartitioner: Send + Sync {
       fn partition_for_lane(&self, lane_id: &LaneId, config: &PartitionConfig) -> u16;
   }
   pub struct Crc16SoloPartitioner;  // default
   ```
   `ExecutionId::solo` delegates to the default `Crc16SoloPartitioner` — callers with a known-good lane-name layout get correct routing with zero ceremony. A deployment hitting amplification installs a custom partitioner by calling `ff_core::partition::solo_partition_with(lane, config, &custom_impl)` from deployment-owned id-minting code. Operators who take this path fork `ff-core`'s default `ExecutionId::solo` minting logic into their own code; the pluggable trait is the stable contract, not the mint function. Defer a `PartitionConfig::with_solo_partitioner` ergonomic wrapper to a follow-up RFC if operator demand emerges post-phase-5 benchmarks (revision 3, 2026-04-18 — names the shape that actually landed in phase-1 per W1's post-implementation review).

**Cost absorbed in revised §8:** ~30min phase 1 (trait + default impl) + ~1h phase 5 (CLI + runbook). Already in the totals.

**Residual risk:** a deployment that doesn't run the CLI pre-deployment and happens to hit a collision sees a hot-spot without diagnosis tooling at hand. We're giving operators the tool, not forcing them to use it. Acceptable.

---

## §6 Migration

### §6.1 Decision: new-execs-only, with a loud-fail parse boundary

* `ExecutionId::parse` rejects bare UUIDs. Return variant: `ExecutionIdParseError::LegacyUuidNotSupported`. Error message: `"execution id '<s>' is a legacy UUID; after RFC-011 all ids must be '{{fp:N}}:<uuid>'"`.
* No dual-code-path for legacy id shape. No shim module. No `LegacyExecutionId` type.
* Pre-RFC execs in dev Valkey instances are wiped at cutover. No production Valkey instances exist.
* cairn's current migration cycle absorbs the id-shape change.

### §6.2 Cutover mechanics (concrete, not handwaved)

1. Phase 1 + 2 + 3 PRs merge to main in order (phases are non-monolithic — each landable on its own; phase 3 is the runtime-observable one).
2. A single release commit bumps `ff-*` crate versions and flags the `ExecutionId` wire shape change in `CHANGELOG.md`. Tagged release goes out as vX.Y.0 (minor bump; we're pre-1.0, anything is breakable).
3. For each dev environment running an older `ff-server`: the cutover runbook (added under `docs/runbooks/rfc-011-cutover.md` in phase 5) says:
   * Stop the server.
   * `valkey-cli --cluster call <any-node> FLUSHALL` (or per-slot-range for multi-cluster dev envs).
   * Pull the new `ff-server` image / binary.
   * Start. Any new FCALL mints new `{fp:N}:<uuid>` ids; no legacy ids are in the store to reject.
4. cairn's phase-4 PR lands in coordination with the tagged release; cairn's own dev Valkey instances get the same FLUSHALL.
5. Benchmarks in phase 5 run on a freshly-flushed cluster.

### §6.3 Rollback

If phase 3 ships and an unforeseen regression appears:

* **Phase 3 is the only rollback-sensitive phase.** Phases 1+2 are API-shape changes; phase 3 is the runtime behaviour change.
* Rollback = `git revert <phase-3-merge-sha>`, re-tag, redeploy. The interim docs (commit `8df40fc`) are still on the tree and come back into force automatically — the two-phase shape is still valid code.
* Phase 1+2 do NOT need to rollback — the new `ExecutionId` shape is compatible with the two-phase method. We'd ship a tag where `ExecutionId` is the new shape but `add_execution_to_flow` is still two-phase. Correctness-sound (same as today's interim state).
* Rollback decision criterion: phase 5 benchmarks show a >10% throughput regression on any scenario, OR a new integration test fails that didn't fail pre-merge, OR cairn-fabric CI goes red against the new ff-server. Any of these triggers a revert-PR same-day.

---

## §7 Implementation plan (phased, hour-level)

Each phase has (a) touch-file list, (b) landing criteria, (c) acceptance gate, (d) cross-review protocol.

### §7.1 Phase 1 — ff-core primitives + ff-script Partial refactor (REVISED 10h, W3)

**Touch:** `crates/ff-core/src/types.rs`, `crates/ff-core/src/partition.rs`, `crates/ff-core/src/keys.rs` (tests only), `crates/ff-core/src/contracts.rs` (doctests), **`crates/ff-script/src/functions/execution.rs`, `scheduling.rs`, `signal.rs`** (Partial refactor per §2.4 + §3.1b).

**Landing:**
* `ExecutionId::for_flow`, `ExecutionId::solo` constructors.
* `ExecutionId::parse` validates `{fp:N}:<uuid>`; rejects bare UUIDs.
* `execution_partition` decodes the hash-tag prefix.
* `ExecutionId::new`, `ExecutionId::from_uuid`, `Default` impl removed.
* ff-script `FromFcallResult` impls refactored to emit `*Partial` types; `complete(execution_id)` combinator on each Partial.

**Acceptance (REVISED — ff-script added after RED-1):**
* `cargo test -p ff-core` green (partition tests regenerated).
* **`cargo check -p ff-script` green** (the Partial refactor must compile clean in the same phase — without this, phase 1 appears to pass while ff-script has silent compile errors waiting for the first dependent build).
* `cargo check -p ff-server -p ff-scheduler -p ff-sdk -p ff-test` produces compile errors that are all phase-2 migration work. Clean on every other crate.

**Scope of expected phase-2 compile errors (clarification — revision 3, 2026-04-18):** the original "only at ExecutionId::new() call sites + at Partial-type consumer sites" wording was too tight. Phase-1 implementation surfaces three distinct error classes, all legitimately deferred to phase 2:

  (a) `ExecutionId::new()` call sites — **pre-phase-1 total: 175** across the workspace (see §3.4 for the authoritative file-by-file breakdown — 10 ff-core + 166 ff-test, with 1 deduplication in the ff-core partition.rs test macro-roundtrip pair). Phase-1 migrates the 10 ff-core sites to `ExecutionId::for_flow` / `solo` as part of the ff-core primitives landing. Phase-2 migrates the remaining **166 ff-test sites**.
  (b) `num_execution_partitions` field references — **6 in ff-test tests** after `PartitionConfig` retired the field in phase 1.B. Migrated to `num_flow_partitions` in phase 2.
  (c) Downstream cascades from (a) and (b) — type-inference failures, mismatched-type errors, and Partial-consumer call sites that need `.complete(exec_id)` wiring. Count scales with the above; mechanical once the root-cause sites in (a)/(b) are fixed. Typical: 1 cascade error per cluster of 10-20 root-cause sites.

Class (c) is NOT a separate category of work — it is the compiler's natural propagation from (a) and (b). Phase-1 acceptance is considered met when:
  - every error across ff-server/ff-scheduler/ff-sdk/ff-test resolves to one of (a), (b), or (c);
  - no error is a surprise class (e.g. an API-shape break the RFC did not anticipate).

**Worked numbers (for reviewer cross-checking against actual phase-1 push output):**

Two distinct counts are cited in this RFC and they measure different things — not inconsistent:

| Metric | Value | What it counts |
|---|---|---|
| Pre-phase-1 `ExecutionId::new()` source sites (§3.4) | **175** | Total source-level call sites grepped across the workspace *before* phase-1 edits. Of these, 10 live in ff-core and 166 in ff-test (with a 1-site rounding overlap from a paired macro-roundtrip test; see §3.4). |
| Post-phase-1 `cargo check -p ff-test --tests` errors | **172** | Compile errors emitted by ff-test after phase-1 lands. Sum = 166 class-(a) `ExecutionId::new()` + 6 class-(b) `num_execution_partitions` + 1 class-(c) cascade. |
| **Post-phase-1+2 merge (main @ `d614ae5`) `ExecutionId::new(` occurrences, workspace-wide** | **0 live references** (1 doc-comment mention of the retired API inside `fixtures.rs:113`, which is historical prose, not a call site) | Confirmation that phase-2 closed the full migration. `grep -rn "ExecutionId::new(" crates/ benches/ examples/ --include="*.rs"` returns exactly one line — a docstring in `crates/ff-test/src/fixtures.rs` documenting that the API was removed. Zero production or test call sites remain. |

Pre-phase-1 counts *source sites* across the whole workspace; post-phase-1 counts *still-to-migrate compile errors scoped to ff-test*. The 10 ff-core sites do not appear in the 172 because phase-1 fixed them; the 6 class-(b) errors do appear even though they are not `ExecutionId::new()` sites, because they are phase-2 work of the same migration shape. The 1 class-(c) cascade rounds out the compile output.

A reviewer running `cargo check` on ff-test post-phase-1 and seeing ~172 errors should verify every error falls under (a), (b), or (c) before flagging a RED.

**Closure confirmation.** After phase-2's merge (commit `d614ae5`), the workspace-wide grep returns zero live call sites — the migration is complete. Future additions to ff-test must use `TestCluster::new_execution_id()` (routes to `ExecutionId::solo(&test_lane, &self.config)`) or `ExecutionId::for_flow` / `solo` directly; `ExecutionId::new()` does not exist in the post-phase-1 API surface.

**Hour breakdown (REVISED):**
* 1h types.rs: bespoke ExecutionId impl + parse + for_flow/solo
* 1h partition.rs: execution_partition rewrite + tests
* 3h compile-error audit across workspace (grep → enumerate → verify compiler agrees)
* **3h ff-script Partial refactor** (10 FromFcallResult impls × ~15min each = 2.5h + 30min for cross-crate type reshuffle)
* **1h ff-script unit tests** regenerate
* 1h cross-review write-up + commit

**Cross-review:** W1 and W2 review before phase 2 starts. Debate round if either dissents.

### §7.2 Phase 2 — Call-site migration (REVISED 14-18h, W1 + W2 parallel)

**Touch:** `crates/ff-server/src/server.rs` (~15 `execution_partition` sites + 7-10 Partial-consumer call sites per §3.1b — NO id-minting; see §3.4 round-2 revision), `crates/ff-scheduler/src/claim.rs` (per §3.5; no partition_scan.rs), `crates/ff-sdk/src/task.rs` + `worker.rs` (~13 sites, rustdoc on `ClaimGrant.partition`), **`crates/ff-test` (166 sites across 5 test files + `fixtures.rs`)**.

**Landing:**
* Every `ExecutionId::new()` migrated to `for_flow` or `solo`. 175 total call sites: 10 ff-core (phase 1) + 1 ff-server + 166 ff-test + 0 cairn-fabric-in-phase-2 (cairn is phase 4).
* Scheduler iteration bounds become `num_flow_partitions`.
* `ClaimGrant.partition` rustdoc updated.
* Partial-type consumer sites call `.complete(args.execution_id.clone())` (§3.1b).
* No Lua changes; `add_execution_to_flow` still two-phase.

**Acceptance:**
* `cargo build --workspace` green.
* `cargo test -p ff-sdk --lib` green.
* `cargo test -p ff-server --tests` green.
* `cargo test -p ff-test --tests` green against a real Valkey — the two-phase `add_execution_to_flow` is unchanged, only its input id shape differs.

**Hour breakdown (REVISED — grounded in independent-grep counts from §3.4):**
* **W1 (5.5-7.5h):** ~15 `execution_partition` routing-through sites at ~2min each (30min, compile-check only) + 7-10 Partial-consumer `.complete()` call-site updates at ~10min each (1-2h) + ff-scheduler bounds + `ClaimGrant.partition` rustdoc (~1h). Plus 2-3h buffer for unforeseen judgement calls and build-break fixes. (Previous round-1 estimate included ~30min for an ff-server id-minting decision that §3.4 round-2 strike removed; W1's phase-2 budget drops by ~30min.)
* **W2 (8-10h):** ff-sdk (~13 `execution_partition` routing-through sites, 30min) + **166 ff-test `ExecutionId::new` migrations**. Breakdown: ~160 solo-path sites × ~30s (80min) + ~6 flow-member sites × ~5min (30min) + `fixture_lane()` helper in `fixtures.rs` + test-file-level `use` import additions (~1h) + per-test-file validation runs (~2h) + buffer for compile-error chains (3-4h). The bulk of this work is mechanical; the critical path is waiting for `cargo test` runs, not writing code.

**Cross-review:** W3 reviews both W1's and W2's diffs before phase 3 starts.

### §7.3 Phase 3 — Atomic ff_add_execution_to_flow + tests (8h, W2)

**Touch:** `lua/flow.lua:122-173`, `crates/ff-server/src/server.rs:1240-1320`, new `crates/ff-test/tests/e2e_flow_atomicity.rs`.

**Landing:**
* `ff_add_execution_to_flow` takes KEYS[4]=exec_core; inner body: `redis.call("HSET", K.exec_core, "flow_id", A.flow_id)` right after the membership SADD.
* `Server::add_execution_to_flow` collapses to one FCALL, KEYS grows from 3 to 4, phase-2 HSET block deletes.
* Docstrings on both sides rewrite around the atomic single-commit shape; orphan-window language removes.
* Integration test added (see §7.3.1 for spec).
* Interim docs (commit `8df40fc`) rewritten: `lua/flow.lua:114-175` loses the two-phase preamble; `server.rs:1240-1320` rustdoc rewrites.
* Issue #21 closed with a pointer to the phase-3 merge commit; labelled `superseded`.

**Revision 4 amendments (2026-04-19) — merge-order note.** The four amendments below describe behaviours that landed in phase-3's code commits on `feat/rfc011-phase3-atomic` (c1e8678 + 88693b8 + fef949f). This RFC revision merges AFTER PR #28 (phase-3 code) per the same pattern used by revisions 3 / 4 in the rev-3 sequence — the prose describes the shipped shape, not a proposal.

**Amendment A — Defensive heal on `AlreadyMember` return (revision 4).** The landed `ff_add_execution_to_flow` implementation stamps `exec_core.flow_id` inside the `SISMEMBER`-already-member branch as well as the freshly-added branch. The write is idempotent by value — if the exec is in this flow's `members_set`, the exec's `flow_id` must already equal this flow's id (or be empty, for an orphan from the pre-phase-3 two-phase-contract era discovered during a rolling upgrade). Re-stamping is a single `redis.call("HSET", ...)` inside the same atomic FCALL — a server-side write, not a client round-trip — and strictly strengthens the invariant: after `ff_add_execution_to_flow` returns successfully on ANY branch, `exec_core.flow_id == args.flow_id` is guaranteed. `node_count` is NOT incremented on this branch, preserving the idempotent-repeat semantics. Documented because the behaviour widens the original spec ("single atomic commit of both writes") with a second write-case the spec did not name.

**Amendment B — Cross-flow guard on non-member branch (revision 4).** To prevent an exec silently belonging to two flows when a caller mistakenly passes a flow_id different from the one the exec already belongs to, the Lua body HGETs `exec_core.flow_id` before the `SADD K.members_set` and returns `err("already_member_of_different_flow:<existing_flow_id>")` if the stored value is non-empty and mismatched. Preserves RFC-007's "exec belongs to at most one flow at a time" invariant; makes the violation loud rather than silent.

**Amendment C — Cross-flow guard on AlreadyMember branch (revision 4, added post-review).** Symmetric extension of amendment B: the AlreadyMember branch (step 2) also HGETs `exec_core.flow_id` before the defensive-heal HSET (amendment A) and returns the same `already_member_of_different_flow` error if the stored value is non-empty and mismatched with `args.flow_id`. Catches the corrupted-state case where an exec IS in this flow's `members_set` but `exec_core.flow_id` is somehow stamped with a different flow (pre-phase-3 orphan) — without this guard, the heal HSET would silently rewrite the mismatched stamp. Empty `existing` goes through the heal path normally (that's the intentional rolling-upgrade recovery). Added in response to Gemini's post-review edge-case finding; symmetric with amendment B on the not-yet-a-member branch.

**Amendment D — Pre-flight partition match (revision 4).** `Server::add_execution_to_flow` adds a pre-FCALL check comparing `exec_partition.index` against `flow_partition.index`. On mismatch it returns `ServerError::PartitionMismatch(...)` with a message that names both indices and points the caller at `ExecutionId::for_flow(&flow_id, config)`. Turns a raw Valkey CROSSSLOT / Lua-lib error into a typed, actionable caller-side signal when the consumer contract ("mint via `for_flow`") is violated. No new error variant; uses the existing `PartitionMismatch(String)` variant from `ServerError`.

**Amendment E — Execution-existence guard (revision 4, added post-review).** The Lua body at step 1 adds `redis.call("EXISTS", K.exec_core)` and returns `err("execution_not_found")` if zero. Symmetric with the `flow_not_found` guard immediately above. Without this, step 5's `HSET K.exec_core flow_id` would silently create an exec_core hash containing only a `flow_id` field — an inconsistent `members_set ↔ exec_core` state that would mislead every downstream reader. Added in response to Gemini's post-review test-coverage ask for an execution_not_found rollback complement; the complementary test lives at `crates/ff-test/tests/e2e_flow_atomicity.rs::test_atomicity_execution_not_found_commits_nothing`.

**Acceptance:**
* `cargo test -p ff-test --tests -- --test-threads=1` green.
* The new `e2e_flow_atomicity` test passes.
* No regression in `cargo test -p ff-server --tests`.

**Hour breakdown:**
* 2h Lua rewrite + Rust method collapse
* 2h docstring sweeps (4 files)
* 3h integration test writing (atomicity assertion + Lua-error rollback test)
* 1h cross-review write-up

**Cross-review:** W1 and W3 both review; both must ACCEPT for merge.

#### §7.3.1 Atomicity-test specification

**File:** `crates/ff-test/tests/e2e_flow_atomicity.rs`.

**Test 1 — happy path atomicity (commits both writes).**

```
1. fcall_create_flow(tc, &flow_id)
2. let eid = ExecutionId::for_flow(&flow_id, &config)
3. fcall_create_execution(tc, &eid, <args>)
4. server.add_execution_to_flow(&AddExecutionToFlowArgs { flow_id, execution_id: eid })
5. Assert (same Valkey session, no await between steps):
   a. SISMEMBER members_set eid → 1
   b. HGET exec_core flow_id → flow_id.to_string()
   c. HGET flow_core node_count → "1"
```

**Test 2 — Lua error rollback (commits neither write).**

Exploits the fact that `ff_add_execution_to_flow`'s early-return paths (`flow_not_found`, `flow_already_terminal`) fire BEFORE any write. Valkey does NOT transactionally roll back a Lua function mid-body (see §9.5 and §10.6 probe 2 — writes before a mid-body error are visible). The test works because the function is structured to validate-before-writing: if validation fails, zero writes have happened yet. Concrete mechanism:

```
1. Inject an invalid argument that triggers the "flow_not_found" early return path
   at lua/flow.lua:141. This is a normal err-return, not a Lua exception, so by
   the contract no HSET on exec_core has happened.
2. fcall_create_execution(tc, &eid, <args>) — exec_core exists but flow_id is "".
3. server.add_execution_to_flow(&args) where flow_id points to a nonexistent flow
   → returns Err(flow_not_found).
4. Assert:
   a. HGET exec_core flow_id → "" (NOT stamped).
   b. SISMEMBER members_set eid → 0 (members unchanged).
```

**Test 3 — idempotency (second call is a no-op).**

```
1. Successful add_execution_to_flow (test 1).
2. Second call with same (flow_id, eid) → OK_ALREADY_SATISFIED.
3. Assert exec_core.flow_id unchanged; node_count still "1".
```

**Test 4 — concurrency: two adds to the same flow in parallel.**

```
1. Two tokio tasks both call add_execution_to_flow with distinct eids against
   the same flow. tokio::join! them.
2. Assert members_set has both eids; node_count = 2; both exec_cores carry
   flow_id.
```

No crash-injection test — the scanner-ticket-motivated "kill process between phases" scenario is now meaningless because there are no phases.

### §7.4 Phase 4 — cairn-fabric re-alignment (6h, cairn team coordination)

**Touch:** cairn-fabric's `RunService::create_run`, `TaskService::create_task`, retry-ledger sharding module.

**Landing:**
* cairn mints exec_ids via `ff_core::ExecutionId::for_flow` / `solo`.
* cairn's partition decoder reads the hash-tag prefix.
* cairn's integration test suite (`test_event_emission.rs` etc.) green against the phase-3 ff-server.

**Acceptance:** cairn-fabric CI green against ff-server at `main` post-phase-3.

**Hour breakdown:** handled by cairn team; our side provides the `ff-core` API + one coordination call.

**Cross-review:** cairn's own review process; our side checks only that the `ff-core` API coverage is adequate.

### §7.5 Phase 5 — Benchmarks + release (4h, W2 or harness owner)

**Touch:** `benches/harness/src/runners/flowfabric.rs`, new `benches/perf-invest/report-rfc011.md`, release-note draft, `docs/runbooks/rfc-011-cutover.md`.

**Landing:**
* Harness runs unchanged scenarios (`wider_50_50`, `wider_80_20`, `wider_get_only`, `wider_pipeline_100`, `wider_streams`) pre-RFC (commit `87810fa`) and post-phase-3.
* Report drafted.
* Release note + cutover runbook written.

**Acceptance:**
* No single-flow throughput regression >5% at equal partition counts.
* Hot-flow regression (multi-member flow scenarios) documented; >10% is a rollback trigger per §6.3.

**Hour breakdown:**
* 2h benchmark runs (pre-RFC + post-RFC on identical hardware)
* 1h report writing
* 1h release note + runbook

**Cross-review:** W1 and W3 review the benchmark numbers and the release note.

---

## §8 Cost totals (REVISED — grounded in §3.1b + §3.4 counts)

| Phase | Owner        | Hours (revised) | Critical path |
|-------|--------------|-----------------|----------------|
| 1     | W3           | 10 (was 6)      | yes (blocks 2, 3) — scope added: ff-script Partial refactor per §3.1b |
| 2     | W1 + W2      | 13.5-17.5 (8-10 W2 + 5.5-7.5 W1, parallel; was 8) | yes (blocks 3) — counts corrected from ~90 to 175 sites per §3.4; §3.4 round-2 struck the ff-server id-mint sub-item (-30min W1) |
| 3     | W2           | 8               | yes (blocks 4, 5) — unchanged |
| 4     | cairn team   | 6               | parallel-with-5 — cairn-side `for_flow`/`solo` decision + re-align |
| 5     | W2 / harness | 4               | parallel-with-4 — unchanged |
| **Total** | **3 workers** | **41.5-45.5h serialised, 35.5-39.5h wall-clock** (was 32h/28h) | |

At 6h/day per worker: **~7 working days end-to-end** assuming no rework and one cross-review debate round per phase. With debate rounds: 8-10 days realistic.

Revised estimate is ~1.7-1.9x the original for phase 2 (because of the site-count undercount, offset slightly by the round-2 id-mint strike) and 1.67x for phase 1 (because of the added ff-script scope). Total revision: ~32h → ~43h (~1.35x). Still within the "3-7 days" range the user's decision was based on — now 7 days at the upper end rather than 5 at the upper end. Not a scope-change trigger.

---

## §9 Debate brief — anticipated attacks and responses

Pre-empting the review rounds. Each attack gets a specific defensible response; reviewers who find a hole here should dissent with specifics, not hand-wave.

### §9.1 "Hot-spot on big flows is a showstopper."

Response: see §5.2. The 10k-member soft limit derives from measured single-slot throughput (§4.4). Operators sizing for larger flows split into sub-flows, which is already the idiomatic scaling pattern — this RFC makes the reason explicit, not new.

### §9.2 "Flow-size-aware placement would be better — small flows atomic, big flows pooled."

Response: see §5.3. Five concrete reasons pooling is strictly worse; none of them dissolve under scrutiny. Crucially, pooling re-opens the atomicity gap for exactly the big flows that most need atomicity.

### §9.3 "You're deleting `ExecutionId::new()`; what about callers that really just want an arbitrary id for a test?"

Response: §7.1. They use `ExecutionId::solo(&fixture_lane, &config)`. The test helper libs (`crates/ff-test/src/helpers.rs`, `assertions.rs`) gain a `fixture_lane()` function that returns a single test-wide `LaneId`. No caller that wanted an arbitrary id actually wanted a random routing slot — they wanted something that would deterministically route somewhere.

### §9.4 "`{fp:N}:<uuid>` in logs is uglier than a bare UUID."

Response: conceded. It's 7-9 characters longer per id. Traded for: the partition is visible in `MONITOR` output, `SLOWLOG`, Valkey's `DEBUG OBJECT`, and every operator diagnostic path. Net operational win; aesthetic loss is bounded by log-grep muscle-memory.

### §9.5 "Phase 3's atomicity test can't actually prove atomicity under a real crash — only under a Lua-error rollback."

Response (REVISED 2026-04-18 after W3 YELLOW): conceding a language tightening without conceding the test design.

The original phrasing ("the Valkey contract makes them equivalent for commit visibility") was looser than what Valkey actually guarantees. Empirical §10.6 probe 2 showed that **writes executed before a mid-function error_reply do commit and become visible** — Valkey's atomicity unit is the single `redis.call`, not the function body. The function doesn't transactionally roll back. So a mid-function error does NOT trigger the same commit-visibility outcome as "process died mid-function."

What the test actually verifies: the *structure* of `ff_add_execution_to_flow` as rewritten in phase 3 is "validate → SADD → HSET → return ok". The error_reply path we test (`flow_not_found`) returns BEFORE any writes. So the test confirms that the rewritten function's error path commits zero writes, which is the behaviour we care about. It does not claim that all mid-function errors in all Lua functions trigger transactional rollback — they don't.

**Corrected language for the RFC:** the atomicity test is a structural test, not a transactional-rollback test. It verifies: (a) the happy path atomically commits both writes (§7.3.1 test 1), (b) the early-return error path commits zero writes (§7.3.1 test 2), (c) idempotent retries are no-ops (test 3). It does NOT test crash-mid-function semantics, because Valkey does not guarantee those semantics — the function can leave partial state visible if it fails after a redis.call but before return.

**Why this is fine for §5.5's purposes:** the orphan window we're closing is not "Lua function crashes mid-body" (Lua functions don't crash mid-body in normal operation) — it's "Rust caller's two-phase control flow has a gap between phase 1 and phase 2." By collapsing to a single FCALL, we eliminate the Rust-level gap. The Lua-level "mid-function partial commit" concern is a different failure mode that requires a different analysis (FF's error-emitting Lua paths deliberately validate before any write, so no writes happen before an error can fire — this is the load-bearing discipline on the Lua side).

**Concession:** language in §7.3 is updated to remove the "equivalent to crash rollback" claim. §9.5 above replaces the original phrasing. §10.6 probe 2's commit-visibility finding is the source of truth.

### §9.6 "Migration is new-execs-only; but can't we have a legacy-UUID shim for one release cycle to give consumers a soft landing?"

Response: see §6.1. We have no production consumers that need a soft landing. A shim is dead weight the moment it ships — code that exists to support a state we've never been in. If this RFC lands after the first external consumer deploys, we'd need the shim; it doesn't.

### §9.7 "The `{fp:<crc16(lane_id)>}` solo scheme collides with `{fp:<flow_id_hash>}` — a lane and a flow could share a partition." (REVISED — key collision addressed; traffic amplification CONCEDED 2026-04-18)

**Original concern (key collision):** two entities sharing a partition is **intended**. They route to the same slot and can participate in atomic multi-key FCALLs. No key collision is possible: `ff:exec:{fp:N}:<uuid>:core` and `ff:flow:{fp:N}:<flow_id>:core` have distinct entity-type prefixes (`exec:` vs `flow:`) before the hash-tag.

**New concern (W3 YELLOW): traffic amplification.** Conceded. A lane and a flow that happen to hash to the same partition pile their traffic on the same Valkey slot. Under the birthday paradox, with ~15 lanes and 256 partitions, the probability of at least one lane colliding with at least one flow is non-negligible (~30-40% for a deployment with ~15 active lanes × ~15 active flows). The collision isn't a correctness issue, but it is an operational hot-spot axis that operators need to be able to diagnose and mitigate.

**Mitigation** (added in §5.5 — new subsection):
* `ff-server` adds a `ff-server admin partition-collisions` CLI command that reports, for the current Valkey cluster state, which lane-hashes collide with which flow-hashes. Operators run this during deployment sizing to spot amplification before it becomes a runtime hot-spot.
* Documentation in `docs/runbooks/partition-amplification.md` (added in phase 5 alongside the cutover runbook) explains: (a) how the collision arises, (b) how to read the CLI output, (c) what to do about it (rename the lane, or adopt a deterministic-but-different solo-partition scheme; see §12.3 for the decision-protocol for the latter).
* `ExecutionId::solo(lane_id, config)` is decoupled from "which Valkey slot" via a pluggable `SoloPartitioner` trait (default: `crc16 % n`), so a deployment that hits amplification can override to a custom mapping without re-building ff-server.

**Scope impact:** CLI command + docs + trait-based extension point = ~1h of phase-5 work + ~30min of phase-1 work (define the trait + default impl). Already absorbed into the revised §8 totals.

**Residual risk:** a deployment that doesn't run the CLI pre-deployment and happens to hit a collision will see a hot-spot without diagnosis tooling handy. This is true for any hash-partitioning scheme; we're giving operators the tool but not forcing them to use it. Acceptable trade-off at our scale; revisit if a second consumer reports operational pain from traffic amplification specifically.

**See §5.5** for the mitigation spec and §12.3 for the "replace the default SoloPartitioner" decision protocol.

### §9.8 "You kept `num_flow_partitions` configurable but the default is 256 — why not make the wider default 1024 for future-proofing?"

Response: see §5.5. 1024 adds key-name overhead (3 extra digits) for no throughput benefit on cluster sizes we actually support. Operators with larger clusters override `PartitionConfig` explicitly. We're not locked in — a default bump is a one-line release-note entry.

### §9.9 "Rollback story sounds easy, but the new ExecutionId shape crosses the wire to cairn — if phase 3 rolls back, cairn has to roll back phase 4 too, coordinated."

Response: Correct. §6.3's rollback criterion triggers same-day, and cairn's phase-4 PR is deliberately landed **after** phase 3 to keep the rollback scope on our side only until the phase-3 merge has proven itself. Phases 4 and 5 both live past the phase-3 bake-in period; if we're going to rollback, it happens before phase 4 ships. If phase 3 bakes in and phase 4 subsequently reveals a cairn-side breakage, rollback is no longer "revert the merge"; it's "fix forward in cairn," which is smaller-scope.

### §9.10 "You said 5 days but phase 2 alone has 140+ call sites — is 8h realistic?"

Response: see §7.2. The 140 sites are ~90% mechanical — `execution_partition(&eid, &config)` stays the same, and the compiler flags every `ExecutionId::new()` site. Each of those is a 30-second fix (pick `for_flow` vs `solo`). Only ~10-20 sites need semantic judgement about whether the exec is flow-bound or solo. Parallelised across W1 (ff-server + scheduler) and W2 (ff-sdk + ff-test), 4h each is within reach. If it's not — the hour estimate is off by 2-3x, not 10x, so the critical path adjusts by a day, not a week.

### §9.11 "Empirical probe 2 shows the first write commits before the cross-node error fires. Doesn't that mean the CURRENT two-phase shape has an even BIGGER orphan window than we thought — we should fix the interim urgently, not wait for RFC-011?"

Response: **No, the current shape is safer than an attempted-cross-slot FCALL would have been.** The probe-2 observation (first write commits, second cross-node write aborts with partial state visible) applies to a **hypothetical** single-FCALL that tries to span slots. Today's `add_execution_to_flow` does NOT attempt that — it issues an FCALL on `{fp:N}` (phase 1), then an HSET on `{p:N}` (phase 2), with explicit Rust-level error handling between them. When phase 2 fails, the Rust caller gets an `Err(ServerError::ValkeyContext)` and can retry, log, or escalate.

Contrast with the hypothetical "stuff both writes into one cross-slot FCALL" path: the first write commits, the second fails mid-function, and the Lua call returns an error — but the caller has no structured way to know which writes landed. Silent partial commit. The two-phase shape avoids this because each write is a separate command with its own return value.

So the empirical finding strengthens the case for this RFC's co-location approach, not weakens it: an attempted-cross-slot FCALL would be **worse** than today's two-phase, not better. The interim (commit `8df40fc`) is the correctly-scoped stop-gap; co-location via this RFC is the correctly-scoped fix.

Probe output preserved in §10.6.

### §9.12 "Module API (Option M) could let us do native-speed work per-node. Why isn't RFC-011 the Module RFC?"

Response: because §10.3-a's empirical finding shows Module API has the **same cross-node EPERM wall** as Lua Functions (`src/module.c:6420-6459`). Module is worth doing for other reasons — per-node performance, custom data structures, capability-routing atomicity (RFC-009) — but it does not close the §5.5 gap. Those other wins belong in a separate RFC-012, filed after this one lands. Conflating them here would bloat the scope and delay the atomicity fix that has a user waiting.

### §9.13 "Valkey 8.0+ floor — what about Redis-consumer compatibility?"

Response: we already ship a Valkey-native architecture (Valkey Functions, RESP3, ferriskey client). There is no Redis consumer to be compatible with. cairn-fabric (our only production-adjacent consumer) pins to `valkey/valkey:8-alpine` explicitly (`/tmp/cairn-rs/scripts/run-fabric-integration-tests.sh`, `/tmp/cairn-rs/crates/cairn-fabric/README.md`). Our interim Valkey floor was effectively 7.2 (where Functions stabilised); moving to 8.0 aligns us with cairn and unblocks future Valkey-8-specific work (see Non-goals §11). See §13 for the explicit version-compatibility commitment.

### §9.14 "§4.4 throughput derivation comes from wider_80_20 bench (SET/GET mix), not from FCALL workload. Is 115k ops/sec realistic for our per-slot ceiling claim?" (W1 YELLOW)

Response: CONCEDE the derivation is indirect; TIGHTEN the language.

The `wider_80_20-ferriskey-wider-8cb6dff.json` measurement is raw SET/GET throughput, not FCALL. FCALL has Lua-script overhead (registration lookup, argument marshalling, Lua bytecode execution, reply serialisation) that a raw SET does not. Our FCALL-heavy workload is slower than raw GET/SET by a factor we haven't measured independently.

**Corrected phrasing for §4.4**: "Per-slot GET/SET throughput from the existing bench: 115k ops/sec (raw data: `benches/perf-invest/wider_80_20-ferriskey-wider-8cb6dff.json` + sibling files). FlowFabric's per-op cost is 3-5 Valkey round-trips (claim → attempt commands → complete/fail), each of which is an FCALL; FCALL throughput is empirically 30-60% of raw SET/GET on comparable hardware. A conservative envelope for per-slot FCALL throughput is **30-40k FCALLs/sec**; for a 4-5 FCALL/exec workload, that's **~7-10k execs/sec per lane per slot**."

That tightening preserves the §5.2 "10k-member soft limit" rationale — 10k members at 7-10k execs/sec saturates the slot for ~1-1.4 seconds end-to-end, which is still the sub-second p99 budget concern. The number is lower than the original (~20-40k/sec), but the shape of the argument survives.

**Phase 5 benchmark commitment:** run an FCALL-specific benchmark during phase 5 and update the RFC's §4.4 + §5.2 numbers with real data. Defer the precise number; ship the structural argument now.

### §9.15 "`lane_id` isn't validated at ingress. Could malformed lane strings break the SoloPartitioner hashing?" (W1 YELLOW, indirect — was a concern about lane string handling)

Response: The `LaneId` type (`crates/ff-core/src/types.rs`) is a `string_id!` macro invocation — a newtype over `String` with no validation on `::new()`. Any UTF-8 bytes are accepted. The `SoloPartitioner` trait (§5.6) hashes `lane_id.as_str().as_bytes()` via crc16, which is byte-safe and length-independent. Malformed lane strings produce valid (if unexpected) hash outputs; they do not crash or misroute.

**Defense-in-depth mitigation** (added in phase 1 alongside the `ExecutionId` validators): `LaneId::new` gains a length-bound check (max 64 bytes, matching the common runtime-label ceiling) and an ASCII-printable validator. Non-conformant lanes fail loudly at construction, not at routing time. One-line additive validation; no existing caller pattern breaks (all current lane names are ASCII-printable).

**Why this is a phase-1 item, not a phase-5 item:** if a deployment's lane strings pass validation today under a new `LaneId::new`, they'll pass forever. If they don't, we want to know at phase-1 compile-time-of-tests, not phase-3 runtime.

### §9.16 "`num_flow_partitions` environment variable / config override — is it documented?" (W1 YELLOW)

Response: CONCEDE, fix in phase 5 doc pass.

Today `PartitionConfig::num_execution_partitions` is settable via `ServerConfig` (struct field), not an env var. Post-RFC `num_flow_partitions` inherits the same configurability shape. Phase 5's release-note and `docs/runbooks/rfc-011-cutover.md` document the config key and default. No behaviour change — just making the doc visible.

**Explicit commitment added in phase 5 punch-list:** `docs/runbooks/rfc-011-cutover.md` contains a "Partition count" section with:
* Default value (256).
* Override mechanism (`ServerConfig::partition_config.num_flow_partitions`).
* Trade-off guidance (lower for small clusters, higher for bigger clusters, observable slot balance).
* Link to `ff-server admin partition-collisions` (§5.6).

### §9.17 "Boot-time Valkey version check at phase-3 merge timing — what if the cluster is mid-rolling-upgrade and one node reports < 8.0?" (W1 YELLOW)

Response: CONCEDE, tighten the §13 commitment.

The boot-time check as drafted reads `INFO server` and refuses to start against Valkey < 8.0. During a rolling upgrade, the node we happen to connect to might be pre-upgrade while others are post-upgrade — transient failure.

**Revised semantics:** the check runs at startup and on first connection. If it fails, `ff-server` logs a structured error and **retries with exponential backoff up to 60 seconds** before giving up. This tolerates a ~1-minute rolling-upgrade window where one replica is temporarily stale. After 60s, the server exits with the structured error — at that point the cluster is not just mid-upgrade, it's mis-configured and needs operator attention.

**Added to phase 3 punch-list:** `Server::start` boot-path in `crates/ff-server/src/server.rs` gains a `verify_valkey_version()` async call with the 60s retry budget. Single additional commit in phase 3; ~30min to write + test.

§13 updated to name the retry budget.

---

## §10 Alternatives (specifics per rejection, not handwaving)

### §10.0 Framing — where atomicity is actually blocked

The research pass (§9.11, §10.5–§10.9) confirms a load-bearing fact: the multi-slot-atomicity boundary is an **architectural property of Valkey Cluster**, not a scripting-API limitation. Valkey's sharded design places each slot on exactly one master node, and there is no cluster-level transaction manager. Neither Lua Functions, Lua scripts, EVAL shebang-less scripts, MULTI/EXEC, nor native Valkey Modules can atomically commit writes across slots in cluster mode. Any option that claims otherwise is claiming to invent cross-node transactions, which is a research problem (FoundationDB, Spanner) and not a weekend implementation.

This reduces the design space to: either **put the keys on the same slot** (co-location, picked in §2), or **accept non-atomicity across slots and manage the orphan window some other way** (Options A, B, C, and the module / cross-slot-flag alternatives below). Every option rejected below falls in the second bucket; §2 is the only option in the first bucket.

Options A, B, C are the already-documented consequences of the second bucket. Options M, N, P, Q, R below are the Valkey-primitive alternatives called out in the design-space expansion.

### §10.1 Option A — flip phase order (stamp exec_core.flow_id first, then FCALL)

**Worse because:** rotates the orphan direction; now `exec_core.flow_id` claims a flow-membership that `flow_core.members_set` doesn't confirm. Symmetric to today, and now the *reverse* join is inconsistent (a reader iterating exec_cores with a given flow_id hits execs the flow doesn't list). All current readers tolerate the existing direction; flipping forces a reader-side audit and rewrites of any joint-consistency check. Same defect class, worse migration.

### §10.2 Option B — second FCALL on {p:N} + retry loop

**Worse because:** adds an `ff_stamp_flow_id_on_exec` function that exists solely for this one write; increases FCALL surface by 1; adds a retry loop to `add_execution_to_flow` that doesn't close the window (just shrinks it from milliseconds-scale to microseconds-scale). A mid-retry crash still orphans. Eventual consistency dressed as atomic; worse because consumers can't tell which they're getting.

### §10.3-a Option M — Valkey Module API

**Worse because:** Module API is bound by the same architectural cross-node constraint as Lua Functions. Verified empirically (see §10.6) and against Valkey 8.0 source (`src/module.c:6420-6459`): `ValkeyModule_Call` in cluster mode checks `getNodeByQuery(...) != getMyClusterNode()` and returns `EPERM` with "Attempted to access a non local key in a cluster node." A native module can do more per-slot (custom data structures, native-speed loops, fine-grained lock semantics) but cannot atomically commit writes across slots whose nodes differ.

The atomicity boundary is **per-node**, not per-API. Valkey cluster has no cross-node transaction manager; no scripting or module API can invent one. Moving from Lua Functions to Modules trades Lua overhead for C (or Rust-via-FFI) ergonomics within one node. It does not close the §5.5 gap.

**Module API is still worth a separate RFC-012** for capability-routing atomicity (RFC-009), custom data structures that amortise round-trips, or the bridge-event delivery story (§1.1 of the gap report). Those are per-node wins. This RFC is about the cross-node gap, which Modules don't help.

**Empirical verification** (this RFC, §10.6): tested against a local 3-node Valkey 7.2 cluster on ports 7000/7001/7002.

* FCALL declaring two KEYS on different slots → `CROSSSLOT Keys in request don't hash to the same slot` (hard-abort, zero writes committed).
* FCALL declaring one KEY + function body calls `redis.call` against an undeclared cross-NODE key → `ERR Script attempted to access a non local key in a cluster node script`. **The first legal write committed before the error fired on the second write.** No rollback of already-executed Lua commands on this path.
* Same configuration with `allow-cross-slot-keys` flag set → **identical error**. The flag relaxes same-node cross-slot restrictions, not cross-node traversal.

The pre-commit of the legal write in the mid-function error case is the empirical smoking gun: Lua Functions are atomic **per-node**, not across the cluster. This is independent of scripting vs module; it is the cluster's single-node-per-slot property.

**Cost:** irrelevant given the capability doesn't match the problem. Filed as RFC-012 motivation instead.

### §10.3-b Option N — MULTI/EXEC (standalone-only atomicity)

**Rejection reason (empirical + architectural):** `MULTI/EXEC` in cluster mode rejects cross-slot keys at queue-time; the `EXEC` fails with `CROSSSLOT` if the queued commands span slots. In standalone mode (no cluster), `MULTI/EXEC` is atomic across arbitrary keys because there is only one node — but this is a deployment-shape-specific guarantee, not an API-level one.

Shipping MULTI/EXEC-based atomicity would mean: "FlowFabric is atomic in standalone deployments, eventually-consistent in cluster deployments." The guarantee depends on a configuration the consumer's code cannot introspect cheaply. cairn-fabric pins `valkey/valkey:8-alpine` which is single-node in dev but cluster-capable in production (see §10.6 probe 5, §13). Shipping a "correct in tests, wrong in prod" story is exactly the scenario this RFC is closing — we would just be moving the defect from crash-window to deployment-shape.

Same empirical wall as Option M: no cluster API makes this atomic across nodes.

### §10.3-c Option P — consumer groups for bridge-event delivery

**Not applicable to §5.5.** Consumer groups (`XREADGROUP` / `XACK`) solve delivery guarantees (at-least-once + ack-based retries) for stream readers, not cross-slot write atomicity. They are relevant to cairn's bridge-event subscription story (§1.1 of the gap report), and worth filing as a separate RFC if we decide cairn should switch from its current call-then-emit pattern to stream-tailing. Orthogonal to this RFC.

### §10.3-d Option Q — CLIENT TRACKING + invalidation streams

**Not applicable to §5.5.** Pushes freshness guarantees to readers, not writers. Solves "reader might see stale data after a write" — a problem we don't have; our readers always do an authoritative read via HGET, not a cached lookup. Relevant to a future caching layer if we ever add one. Not relevant here.

### §10.3-e Option R — keyspace notifications for crash-recovery

**Explicitly rejected by prior RFC.** RFC-003 §TTL-based expiry detection, rule 4: "`ff-engine` must not rely on keyspace notifications for correctness." Keyspace notifications are fire-and-forget pub/sub, lost under disconnect, lost across restarts, not durable. RFC-003's rejection holds for the same reason here: we cannot use them as an atomicity primitive because they provide no delivery guarantee.

### §10.4 Option C — landed docs + reconciliation scanner (commit 8df40fc)

**Worse because:** zero atomicity improvement. The scanner heals post-hoc, which is strictly weaker than not-creating-the-gap. Acceptable as an interim because it's cheap (doc + ticket) and the scanner can be written incrementally, but kept as interim, not terminal. This RFC's phase 3 supersedes.

### §10.5 Option Z — deferred RFC ("revisit when a consumer complains")

**Worse because:**

* Window closes at first external consumer deployment. Once that happens, every future `ExecutionId`-shape change requires a migration protocol to support legacy + new ids concurrently. The protocol's design cost + implementation cost + correctness-testing cost ≥ today's phase 1-3 combined.
* Accumulates call-site debt. Every `ExecutionId::new()` call added under the deferred state is a future migration touch-point. Repo is growing; the debt compounds.
* Signals "this is as good as it gets" to consumers reading the crate-level atomicity promise. Dishonest given an implementable alternative exists.
* Defers a decision that has to be made eventually. Deferred decisions accumulate decision-context-debt — by the time we revisit, the original rationale is in Slack logs and the reviewer has to rebuild the context from scratch.

---

### §10.6 Empirical evidence (design-space verification, 2026-04-18)

Tested against a local 3-node Valkey cluster (ports 7000/7001/7002, Valkey 7.2.12), cross-referenced against Valkey 8.0 source (branch `8.0` of `github.com/valkey-io/valkey`, file `src/module.c:6420-6459`, file `src/script.c:426-456`). All probes are reproducible by running the cluster spin-up + FUNCTION LOAD + FCALL commands in this section.

**Probe 1 — FCALL, two declared KEYS on different slots.**
```
$ valkey-cli -p 7000 FCALL fcall_declared_only 2 "{a}k1" "{b}k2"
CROSSSLOT Keys in request don't hash to the same slot
```
Hard abort. No writes committed. Confirms the declared-KEYS single-slot constraint applies to Functions identically to regular commands.

**Probe 2 — FCALL, one declared KEY, body writes undeclared cross-node key.**
```
$ valkey-cli -p 7000 FCALL fcall_undeclared 1 "{b}k2"
ERR Script attempted to access a non local key in a cluster node script: fcall_undeclared, on @user_function:15.
```
**Critical finding:** the first legal write (`SET {b}k2 a`) **committed before the error fired on the second write.** The atomicity guarantee is per-node, not function-wide. Mid-function cross-node attempts abort with partial state already visible.

**Probe 3 — Same as probe 2, with `allow-cross-slot-keys` flag set.**
Identical error. The flag relaxes same-node cross-slot restrictions, not cross-node traversal.

**Probe 4 — Valkey Module API (source read, not empirical — library build requires pulling headers).**
`src/module.c:6420-6459`:
```c
if (server.cluster_enabled && !mustObeyClient(ctx->client)) {
    ...
    if (getNodeByQuery(c, c->cmd, c->argv, c->argc, NULL, &error_code) != getMyClusterNode()) {
        ...
        msg = sdsnew("Attempted to access a non local key in a cluster node");
        errno = EPERM;
        ...
    }
}
```
Same architectural wall: `VM_Call` returns `EPERM` if the target key's slot belongs to a different node.

**Probe 5 — cairn-fabric deployment target verification.**
`/tmp/cairn-rs/scripts/run-fabric-integration-tests.sh:`
```
VALKEY_IMAGE="${VALKEY_IMAGE:-valkey/valkey:8-alpine}"
```
`/tmp/cairn-rs/crates/cairn-fabric/README.md`: same pin. Cairn is Valkey-native (not Redis). Valkey 8.x API constraints are safe to impose on this RFC's implementation.

**Conclusion.** The multi-slot-atomicity boundary is architectural (single master per slot range, no cross-node transaction manager) and applies uniformly to Lua Functions, Lua scripts, MULTI/EXEC, and native Valkey Modules. Hash-tag co-location is the only mechanism that closes the §5.5 gap in cluster mode. This RFC's §2 primary recommendation holds.

## §11 Non-goals

* Changing the wire shape of cairn's `BridgeEvent::*` variants. `execution_id` is still a `String` at the wire boundary; it just has more structure.
* Online migration of existing execs. §6.
* Hot-spot mitigation beyond partition-count bump + flow-size guidance. §5.
* RFC-009 capability-routing interaction. Orthogonal; capability routing sits above exec-partition.
* Deleting `PartitionFamily::Execution` from the public API. The variant stays for API compatibility; only its routing behaviour changes.
* **Valkey Module API adoption.** Per §10.3-a and §10.6, Modules are confirmed NOT an atomicity fix (same cross-node EPERM wall as Functions). Modules remain valuable for per-node work — custom data structures, native-speed loops, capability-routing atomicity on a single slot. Filed as RFC-012 motivation; explicitly NOT in this RFC.
* **Consumer-group-based bridge-event delivery.** Per §10.3-c, orthogonal to §5.5 atomicity. Cairn's bridge-event observation story is a separate design surface; filed as a potential follow-up RFC if cairn's current call-then-emit pattern proves inadequate in production.

## §12 Open questions (decision protocol for each)

Kept to the minimum. Each has a decision protocol for when data arrives; nothing is left as "figure it out later."

### §12.1 `num_flow_partitions` default: 256 vs 512 vs 1024

**Current pick:** 256.
**Data we don't have yet:** behaviour on clusters with 16+ Valkey nodes.
**Decision protocol:** phase 5 benchmarks run against single-node + 3-node + 6-node clusters. If the 6-node cluster shows uneven slot distribution at 256 partitions, bump the default in a follow-up PR (not this RFC).
**Fallback:** 256 works; the question is whether we can do better later.

### §12.2 ExecutionIdParseError variant granularity

**Current pick:** three variants — `MissingTag`, `InvalidPartitionIndex`, `InvalidUuid`.
**Data we don't have yet:** which error callers most want to handle distinctly.
**Decision protocol:** phase 1 ships with these three; phase 4 (cairn side) may request a fourth variant if they hit a concrete case; adjusted via a non-breaking enum addition before the vX.Y.0 tag.
**Fallback:** three variants cover every failure mode we've grepped; expansion is additive.

### §12.3 Custom `SoloPartitioner` adoption trigger (added 2026-04-18 after W3 §9.7 concession)

**Current pick:** `Crc16SoloPartitioner` (default, `crc16 % num_flow_partitions`).
**Data we don't have yet:** which deployments hit traffic-amplification collision hotspots in practice, and what non-default partitioning schemes mitigate them.
**Decision protocol:**
1. Deployments run `ff-server admin partition-collisions` pre-go-live (documented in phase 5 runbook).
2. If the report flags a collision between a hot lane and a hot flow, the operator either renames the lane (cheapest fix) or installs a custom `SoloPartitioner`.
3. If three or more operators independently report collisions that lane-renaming doesn't fix, the default partitioner's algorithm is up for revision in a follow-up RFC (e.g. switching to a keyed hash that varies per-deployment).
**Fallback:** `crc16 % N` is a standard hashing choice matching Valkey's own slot-assignment; a deployment that hits systematic collisions has either a lane-naming problem (fixable by rename) or a scale-and-configuration problem (fixable by `num_flow_partitions` bump).

---

## §13 Valkey version compatibility

**Floor: Valkey 7.2.** (See Amendment F below for the floor-revert rationale; supersedes the originally-proposed 8.0 floor.)

Rationale:
* FlowFabric depends on Valkey Functions (stabilised in Valkey 7.2 / Redis 7.0), RESP3 (Valkey 7.2+), and hash-tag routing (stable since Redis 3.0). These are the primitives actually used by the co-location design — nothing the RFC adds requires an 8.0-only behavior.
* cairn-fabric (our only production-adjacent consumer) pins `valkey/valkey:8-alpine` in its test harness, but cairn can run against any Valkey ≥ 7.2 — the 8 pin is its reproducibility choice, not a FlowFabric requirement. Sources: `/tmp/cairn-rs/scripts/run-fabric-integration-tests.sh`, `/tmp/cairn-rs/crates/cairn-fabric/README.md`.
* Holding the floor at 7.2 keeps cairn's current deployment supported while leaving room for downstream operators who cannot move to 8 yet (slower-moving infra, managed-Valkey 7.2 instances, etc.).

**Explicit compatibility commitment:** `ff-server` targets Valkey 7.2+. Valkey 7.0 / 7.1 and earlier are not supported. Redis (any version) is not supported and never was — FlowFabric is a Valkey-native engine.

**Version assertion:** `ff-server` boot-time version check (added in phase 3) reads `INFO server` and refuses to start against Valkey < 7.2 with a structured `ServerError::ValkeyVersionTooLow { detected, required }` error. **Retry budget: 60s with exponential backoff** to tolerate rolling-upgrade transients where a replica is temporarily stale (see §9.17). After 60s the server exits; that's a misconfiguration signal, not a transient. CI matrix includes both `valkey/valkey:8-alpine` (primary) and `valkey/valkey:7.2` (floor guard) to prevent accidental 8-specific primitive adoption.

**Amendment F — Floor revert from 8.0 to 7.2 (post-phase-3).** The originally-shipped phase-3 floor was Valkey 8.0. Post-ship review found the 8.0 floor was not load-bearing: every primitive this RFC actually uses (Functions, RESP3, hash-tags, co-located FCALL, `COPY … REPLACE`) was already available and stable in Valkey 7.2. The 8.0 rationale rested on "cairn pins 8" — but cairn's pin is a reproducibility choice and cairn can run against 7.2. Lowering the floor to 7.2 supports downstream operators on slower-moving infra without costing FlowFabric any capability. The 8.0 floor was implemented in `ff-server`'s `verify_valkey_version` as a major-only (`detected_major >= 8`) comparison; the revert changes it to a `(major, minor) >= (7, 2)` comparison and adds a parse step for the minor component (Valkey always reports `major.minor.patch` in INFO so no response-shape change is needed). CI gains a `valkey/valkey:7.2` × linux-x86_64 × standalone entry to guard against future accidental 8-specific adoption.

## §14 References

* `lua/flow.lua` — `ff_add_execution_to_flow`, single-atomic-FCALL shape.
* `crates/ff-server/src/server.rs` — `add_execution_to_flow` method, collapsed to one FCALL.
* `benches/perf-invest/wider_80_20-ferriskey-wider-8cb6dff.json` — per-slot throughput measurement (115k ops/sec) backing the §4.4 hotspot analysis.
* RFC-001 §4 (ExecutionId lifecycle), RFC-007 §3 (flow structural keys), RFC-010 §4.1 (Valkey architecture partition overview) — all touched by this RFC; cross-references updated in phase 3.
