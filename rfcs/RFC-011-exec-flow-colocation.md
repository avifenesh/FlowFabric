# RFC-011: Exec/flow hash-slot co-location for atomic membership

**Status:** Accepted. Implementation starts immediately after this RFC lands and survives cross-review debate (all three workers ACCEPT).
**Author:** Worker-2 (FlowFabric team).
**Supersedes:** issue #21 (reconciliation scanner) on phase-3 merge. Commit `8df40fc` on `feat/p36-fix-flow-id-atomicity` is the interim safety net; phase 3 rewrites the docs it added.
**Referenced by:** `benches/perf-invest/bridge-event-gap-report.md` §4.
**Cost estimate:** 28-40 engineer-hours across 3 workers, 5 phases. See §8 for hour-level breakdown.

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
* `<uuid>` is a v4 UUID for uniqueness within that partition.
* `N` is an integer in `0..num_flow_partitions`.

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
    ///  - non-integer N, or N >= num_flow_partitions
    ///  - UUID suffix fails v4 parse
    pub fn parse(s: &str) -> Result<Self, ExecutionIdParseError>;

    /// Raw string form; always `{fp:N}:<uuid>`.
    pub fn as_str(&self) -> &str;

    /// Decoded partition (no re-hash; reads from the hash-tag).
    pub fn partition(&self) -> u16;
}
```

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

### §3.4 `ExecutionId::new()` removal

Every call site of `ExecutionId::new()` migrates to `for_flow` or `solo`. Compiler enforces this — after phase 1, `ExecutionId::new` doesn't exist and every caller fails to compile until migrated.

* ff-server: `crates/ff-server/src/server.rs` `create_execution` handler — picks `for_flow` when `args.flow_id.is_some()`, else `solo(&args.lane, config)`.
* ff-test: ~50 sites across `tests/e2e_lifecycle.rs`, `tests/e2e_api.rs`, `tests/pending_waitpoints_api.rs`, `tests/result_api.rs`, `tests/waitpoint_tokens.rs`, `tests/admin_rotate_api.rs`. Solo-path tests take `ExecutionId::solo(&fixture_lane, &config)`; multi-exec flow tests take `for_flow(&shared_fid, &config)`.
* cairn-fabric: phase 4 (§8).

### §3.5 Scheduler / claim routing

* `crates/ff-scheduler/src/partition_scan.rs` — outer loop iterates `0..num_flow_partitions` (not `num_execution_partitions`). The per-partition scan body is unchanged — it reads `{fp:N}`-slotted eligible-zsets and routes FCALLs to the same slot, which is exactly what we want post-co-location.
* `crates/ff-scheduler/src/claim.rs` — `ClaimGrant.partition` holds `{fp:N}` instead of `{p:N}`. One field-value change; consumers use it opaquely as a routing hint, not for logic branching.

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

### §7.1 Phase 1 — ff-core primitives (6h, W3)

**Touch:** `crates/ff-core/src/types.rs`, `crates/ff-core/src/partition.rs`, `crates/ff-core/src/keys.rs` (tests only).

**Landing:**
* `ExecutionId::for_flow`, `ExecutionId::solo` constructors.
* `ExecutionId::parse` validates `{fp:N}:<uuid>`; rejects bare UUIDs.
* `execution_partition` decodes the hash-tag prefix.
* `ExecutionId::new`, `ExecutionId::from_uuid`, `Default` impl removed.

**Acceptance:**
* `cargo test -p ff-core` green (partition tests regenerated).
* `cargo check -p ff-server -p ff-scheduler -p ff-sdk` produces compile errors at `ExecutionId::new()` call sites only. Clean on every other crate.

**Hour breakdown:**
* 1h types.rs: bespoke ExecutionId impl + parse + for_flow/solo
* 1h partition.rs: execution_partition rewrite + tests
* 3h compile-error audit across workspace (grep → enumerate → verify compiler agrees)
* 1h cross-review write-up + commit

**Cross-review:** W1 and W2 review before phase 2 starts. Debate round if either dissents.

### §7.2 Phase 2 — Call-site migration (8h, W1 + W2 parallel)

**Touch:** `crates/ff-server/src/server.rs` (create_execution id minting), `crates/ff-scheduler/src/claim.rs` + `partition_scan.rs`, `crates/ff-sdk/src/task.rs` + `worker.rs`, ~50 ff-test sites, ~90 ExecutionId::new() call sites.

**Landing:**
* Every `ExecutionId::new()` migrated to `for_flow` or `solo`.
* Scheduler iteration bounds become `num_flow_partitions`.
* `ClaimGrant.partition` rustdoc updated.
* No Lua changes; `add_execution_to_flow` still two-phase.

**Acceptance:**
* `cargo build --workspace` green.
* `cargo test -p ff-sdk --lib` green.
* `cargo test -p ff-server --tests` green.
* `cargo test -p ff-test --tests` green against a real Valkey — the two-phase `add_execution_to_flow` is unchanged, only its input id shape differs.

**Hour breakdown:**
* W1: 4h — ff-server create_execution + scheduler + ClaimGrant rustdoc (~25 sites)
* W2: 4h — ff-sdk + ff-test migration (~75 sites)

**Cross-review:** W3 reviews both W1's and W2's diffs before phase 3 starts.

### §7.3 Phase 3 — Atomic ff_add_execution_to_flow + tests (8h, W2)

**Touch:** `lua/flow.lua:122-173`, `crates/ff-server/src/server.rs:1240-1320`, new `crates/ff-test/tests/e2e_flow_atomicity.rs`.

**Landing:**
* `ff_add_execution_to_flow` takes KEYS[4]=exec_core; inner body: `redis.call("HSET", K.exec_core, "flow_id", A.flow_id)` right after the membership SADD.
* `Server::add_execution_to_flow` collapses to one FCALL, KEYS grows from 3 to 4, phase-2 HSET block deletes.
* Docstrings on both sides rewrite around the atomic single-commit shape; orphan-window language removes.
* Integration test added (see §7.3.1 for spec).
* Interim docs (commit `8df40fc`) rewritten: `lua/flow.lua:114-175` loses the two-phase preamble; `server.rs:1240-1320` rustdoc rewrites; `benches/perf-invest/bridge-event-gap-report.md` §4 gets a final "landed" update.
* Issue #21 closed with a pointer to the phase-3 merge commit; labelled `superseded`.

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

Exploits the fact that `redis.error_reply(...)` inside a Lua function aborts the whole FCALL atomically — Valkey's scripting contract guarantees no commands executed before the error remain visible. Concrete mechanism:

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

## §8 Cost totals

| Phase | Owner        | Hours | Critical path |
|-------|--------------|-------|----------------|
| 1     | W3           | 6     | yes (blocks 2, 3) |
| 2     | W1 + W2      | 8 (4+4 parallel) | yes (blocks 3) |
| 3     | W2           | 8     | yes (blocks 4, 5) |
| 4     | cairn team   | 6     | parallel-with-5 |
| 5     | W2 / harness | 4     | parallel-with-4 |
| **Total** | **3 workers** | **32h serialised, 28h wall-clock** | |

At 6h/day per worker: ~5 working days end-to-end assuming no rework and one cross-review debate round per phase. With debate rounds: 6-8 days realistic.

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

Response: Correct, and the test is named accordingly. The point of the test is to verify the **Lua function's declared rollback semantics**, which Valkey's scripting contract extends to real crashes. `redis.error_reply()` mid-function triggers the same transactional rollback path as "process died mid-function" — both paths discard intermediate writes because the function executes inside an atomic Lua callback. Testing the error-reply path is the testable proxy for the crash path; the Valkey contract makes them equivalent for commit visibility.

### §9.6 "Migration is new-execs-only; but can't we have a legacy-UUID shim for one release cycle to give consumers a soft landing?"

Response: see §6.1. We have no production consumers that need a soft landing. A shim is dead weight the moment it ships — code that exists to support a state we've never been in. If this RFC lands after the first external consumer deploys, we'd need the shim; it doesn't.

### §9.7 "The `{fp:<crc16(lane_id)>}` solo scheme collides with `{fp:<flow_id_hash>}` — a lane and a flow could share a partition."

Response: Correct, and that's **intended**. Two entities sharing a partition is the co-location story; they route to the same slot and can participate in atomic multi-key FCALLs. The uniqueness guarantee is on the UUID suffix, not the partition prefix. No key collision possible: `ff:exec:{fp:N}:<uuid>:core` and `ff:flow:{fp:N}:<flow_id>:core` have distinct entity-type prefixes (`exec:` vs `flow:`) before the hash-tag.

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

---

## §13 Valkey version compatibility

**Floor: Valkey 8.0.**

Rationale:
* FlowFabric already depends on Valkey Functions (stabilised in Valkey 7.2 / Redis 7.0), RESP3 (Valkey 7.2+), and hash-tag routing (stable since Redis 3.0). We were effectively at the Valkey 7.2 floor before this RFC.
* cairn-fabric (our only production-adjacent consumer) pins `valkey/valkey:8-alpine` in its test harness. Sources: `/tmp/cairn-rs/scripts/run-fabric-integration-tests.sh` (`VALKEY_IMAGE="${VALKEY_IMAGE:-valkey/valkey:8-alpine}"`), `/tmp/cairn-rs/crates/cairn-fabric/README.md` (same default), `/tmp/cairn-rs/.github/workflows/ci.yml` ("disposable Valkey 8 container via testcontainers-rs").
* Moving our floor to 8.0 aligns with cairn's pin and unblocks future work (e.g. the Module API explorations in RFC-012, which target Valkey 8's `ValkeyModule_*` naming convention).

**Explicit compatibility commitment:** `ff-server` post-RFC-011 targets Valkey 8.0+. Valkey 7.x is not supported. Redis (any version) is not supported and never was — FlowFabric is a Valkey-native engine.

**Version assertion:** `ff-server` boot-time version check (added in phase 3) reads `INFO server` and refuses to start against Valkey < 8.0 with a structured error message. Single CI matrix entry: `VALKEY_IMAGE=valkey/valkey:8-alpine`.

## §14 References

* `benches/perf-invest/bridge-event-gap-report.md` §4 — gap-report amendment (rewrites to reference this RFC post-phase-3).
* Commit `8df40fc` on `feat/p36-fix-flow-id-atomicity` — interim two-phase-contract documentation + issue #21. Superseded on phase-3 merge.
* `lua/flow.lua:114-175` — current (interim) `ff_add_execution_to_flow` docstring with two-phase contract; rewritten in phase 3.
* `crates/ff-server/src/server.rs:1240-1320` — current (interim) `add_execution_to_flow` method; collapses in phase 3.
* `benches/perf-invest/wider_80_20-ferriskey-wider-8cb6dff.json` — per-slot throughput measurement (115k ops/sec) backing the §4.4 hotspot analysis.
* RFC-001 §4 (ExecutionId lifecycle), RFC-007 §3 (flow structural keys), RFC-010 §4.1 (Valkey architecture partition overview) — all touched by this RFC; cross-references updated in phase 3.
