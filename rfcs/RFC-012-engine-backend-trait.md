# RFC-012: `EngineBackend` trait — abstracting FlowFabric's write surface

**Status:** Accepted (round-4) + round-5 micro-amendment (peer lease-releasing methods `delay` + `wait_children`) + round-7 amendment (Stage 1b deferral resolutions — `create_waitpoint` add, `append_frame` return widen, `report_usage` return replace; `suspend` deferred to Stage 1d).
**Created:** 2026-04-22
**Supersedes:** issue #89 (trait-extraction plan) on acceptance.
**Related:** issues #87, #88, #90, #91, #92, #93 (concrete follow-up work this RFC authorises).
**Predecessor:** issue #58 Phase 1 — sealed the read surface (`ExecutionSnapshot`, `FlowSnapshot`, `EdgeSnapshot`) and typed the error surface (`EngineError` — landed on `main` at `crates/ff-sdk/src/engine_error.rs` per PR #81).

### Round-5 amendment summary (post-acceptance, Stage-0-blocking)

During the issue #89 Phase-1 method inventory, Worker I discovered two lease-releasing `ClaimedTask` operations that the round-4 inventory did not elevate to peer trait methods: `delay_execution` (`crates/ff-sdk/src/task.rs:414`) and `move_to_waiting_children` (`crates/ff-sdk/src/task.rs:460`). Both are structurally peers of `suspend` — they hand back the lease, run a single FCALL (`ff_delay_execution`, `ff_move_to_waiting_children`) under the fence triple, and leave the attempt in a non-terminal state awaiting a later event (wall-clock time; child-dependency completion). Omitting them would force Stage 1 to ship an incomplete trait and file a follow-up RFC to bolt them on, compounding call-site debt (§1.3 bullet 3).

This amendment adds them as methods 14 and 15 in §3.1.1. Trait count goes from 13 → 15. No other sections change; Stage-0 type inventory (§3.3.0) is untouched (the new methods' signatures reuse existing types: `TimestampMs` from `ff-core::types`, `Handle`, `EngineError`). The amendment is additive to the accepted round-4 shape.

### Round-7 amendment summary (post Stage 1b — issue #117 deferral resolutions)

Stage 1b (shipped `6f54f9b`, PR #119) migrated 8 of the 12 `ClaimedTask` SDK methods onto the `EngineBackend` trait as thin forwarders. Four methods could not land as thin forwarders: three because the trait's return type was strictly less expressive than the SDK's, and one because the trait had no slot at all. Issue [#117](https://github.com/avifenesh/FlowFabric/issues/117) tracks the gap.

This amendment ships **three of the four** deferred methods:

* **`create_waitpoint`** — new trait method (method **16** in §3.1.1). Issues a pending waitpoint; activation happens via a later `suspend` call (Lua `use_pending_waitpoint` ARGV flag). Introduces new type `PendingWaitpoint` in `ff-core::backend`.
* **`append_frame`** — return type widens from `()` to `AppendFrameOutcome { stream_id, frame_count }`. Moves `AppendFrameOutcome` from `ff-sdk::task` to `ff-core::backend` (Stage-0-style type move with `pub use` shim), mirroring the Stage-1a `FailOutcome` precedent.
* **`report_usage`** — return type replaces `AdmissionDecision` with `ReportUsageResult { Ok, AlreadyApplied, SoftBreach, HardBreach }`. `AdmissionDecision` is removed. `ReportUsageResult` gains `#[non_exhaustive]` in the same commit (atomic — no semver-two-step).

**`suspend` is deferred to Stage 1d** (tracked under #117). Round-3 review (Worker M) established that any trait `SuspendOutcome` shape preserving the existing Round-4 `HandleKind::Suspended` contract entangles with the SDK wire parser `parse_suspend_result` at `crates/ff-sdk/src/task.rs:1557-1562`, which constructs `Suspended` exhaustively from wire bytes with no access to `Handle`'s opaque payload. Stage 1d bundles `suspend`'s return-type widening with the input-shape rework (`ConditionMatcher`↔`WaitpointSpec` / typed `ResumeCondition`); round-7 ships what can ship independently.

**Envelope prose.** Round-5 already relaxed the §3.1 "15 in spirit" envelope from round-4's 13. Round-7 relaxes further: trait grows 15 → 16 lifecycle ops (`async fn` count 16 → 17 including `create_waitpoint`). `create_waitpoint` is a gap-fill add, not a split-for-honesty; earlier round-2/round-5 prose is not rewritten, but readers should treat the "in spirit" envelope as "17 current trait methods" as of Round-5+Round-7.

**Shape.** Trait-surface breaking. Widens 1 existing return type, replaces 1 existing return type, adds 1 new method, adds 1 new type in `ff-core::backend`, adds `#[non_exhaustive]` to 1 existing contract enum. No Lua / wire changes.

**Target release: 0.4.0** — `report_usage`'s `AdmissionDecision → ReportUsageResult` swap is unambiguously breaking and forces the minor bump on its own. Lands INDEPENDENTLY of Stage 1c (not a sub-stage; parallel RFC-amendment-only landing).

§3.1.1 (method 16), §3.3.0 (type inventory), §3.3 (signatures), and §5 (stage map) are updated in place. Full content and challenger record are captured in §R7 at the end of this RFC; exploration trail lives in `rfcs/drafts/RFC-012-amendment-117-deferrals.{K,L,M}-challenge.md`.

### Round-4 revision summary (post Worker M CHALLENGE at `183c10f`)

Worker M's round-3 CHALLENGE landed 7 must-fix items grounded in concrete type-existence checks against the tree. All 7 are addressed in-place; the RFC now cleanly separates Stage 0 (type plumbing + `EngineError` broadening) from Stage 1 (trait extraction), and fuses the former Stage 1 + Stage 2 into one stage per M6.

* **M1 (Transport variant shape mismatch):** §4.2 now matches code verbatim (`Transport { source: Box<ScriptError> }`). §5 adds a Stage 0 prerequisite: broaden `Transport` to `Transport { backend: &'static str, source: Box<dyn std::error::Error + Send + Sync> }` before Stage 1 begins, tracked as a named Phase-1-scope PR.
* **M2 (`EngineError` variant list incomplete):** §4.2 now cites `engine_error.rs` authoritatively and enumerates all current variants verbatim (`NotFound`, `Validation { kind, detail }`, `Contention`, `Conflict`, `State`, `Bug`, `Transport`) plus the Stage-0 additions (`Unavailable`, broadened `Transport`).
* **M3 (~13 undefined types):** new §3.3.0 enumerates every type the §3.3 sketch references, splits them into "exists-in-tree" (4 types) vs "Stage-0 new-type deliverable" (13 types), names a target crate + approximate shape for each, and corrects the §5.1 landing-gate wording. Stage 1 is no longer described as "zero public-surface change"; Stage 0 is the public-surface-additive stage.
* **M4 (`ResumeSignal` in wrong crate):** Stage 0 moves `ResumeSignal` from `ff-sdk/src/task.rs` to `ff-core::contracts`. Breaking-change profile documented: consumer re-exports from `ff_sdk::task::ResumeSignal` at old path via `pub use` during the 0.4.x window.
* **M5 (`HandleKind` visibility unspecified):** §4.1 commits to `#[non_exhaustive] pub enum HandleKind`; consumers must include a `_` arm.
* **M6 (Stage 1+2 force double migration):** former Stage 1 and Stage 2 fuse into a single "Stage 1: trait + backend-config" landing. §5 renumbered; §9 sequencing updated.
* **M7 (§3.4.1 outbox inconsistency):** outbox is now **REQUIRED** for non-transactional backends; §3.4.1 updated. The "never-pre-commit" invariant is kept mandatory; outbox is the mechanism non-transactional backends MUST use to satisfy it.

M's deserves-debate items D1–D5 are addressed in §7 (new §7.17–§7.19) and §8 (new §8.21–§8.23). D3 (single terminal method) is recorded as a rejected alternative in §6.6.

### Round-2 revision summary (post Worker K CHALLENGE, retained for trail)

Worker K's first-round CHALLENGE landed 8 must-fix items; the orchestrator corrected #1 (K read a stale checkout — `EngineError` does exist in `crates/ff-sdk/src/engine_error.rs`). Items #2–#8 drove the following RFC revisions:

* **#2:** split `resume` into `observe_signals` + `claim_from_reclaim` (§3.1 item 8).
* **#3:** `claim` for fresh work; `claim_from_reclaim` for resumed; `claim_via_server` explicitly not on the trait.
* **#4:** §3.4.1 distinguishes commit atomicity from notification atomicity.
* **#5:** dropped five associated types; concrete `Handle`/`EngineError`/`Pin<Box<dyn Stream>>` at the trait boundary (§4).
* **#6:** split `progress` Both-variant into `progress` + `append_frame` (13 methods).
* **#7:** `Err(EngineError::Unavailable { op })` replaces `unimplemented!()` in staged backend skeletons.
* **#8:** Stage 5 seal reframed per owner's cairn-no-gate decision.

---

## §1 Motivation

### §1.1 Where we are today

FlowFabric's SDK and engine crates are fully coupled to Valkey. Every state-mutating operation a worker performs is a hand-rolled Lua FCALL, constructed by building a positional `KEYS` + `ARGV` array against a concrete `ff_core::keys::ExecKeyContext`, shipped through `ferriskey::Client::fcall(...)`, and parsed back through `ff-script`'s `FromFcallResult` layer. `crates/ff-sdk/src/task.rs` is the canonical example: 18 methods on `ClaimedTask`, each constructing its own KEYS/ARGV block and dispatching to a named Lua function.

Phase 1 of the decoupling effort (issue #58) closed the **read** half of that coupling. `FlowFabricWorker::describe_execution`, `describe_flow`, and `describe_edge` now return domain snapshots — `ExecutionSnapshot`, `FlowSnapshot`, `EdgeSnapshot` in `ff_core::contracts` — that have zero Valkey shape leaking into them. The error surface gained a typed `EngineError` enum with `Validation` / `Contention` / `Conflict` / `State` / `Bug` buckets, mapped deterministically from `ScriptError` and `ferriskey::ErrorKind` via `classify()`.

### §1.2 What remains coupled (the leak inventory)

Worker F's 2026-04-22 audit enumerated the residual leaks. Six classes remain open:

1. **The 18 `ClaimedTask` FCALL methods.** `delay_execution`, `move_to_waiting_children`, `complete`, `fail`, `cancel`, `renew_lease`, `update_progress`, `report_usage`, `create_pending_waitpoint`, `append_frame`, `suspend`, `resume_signals`, plus the dispatch sub-cases inside `fail` (retry-scheduling branches) and the claim family on `FlowFabricWorker` (`claim_next`, `claim_from_grant`, `claim_via_server`, `claim_from_reclaim_grant`). Each constructs KEYS/ARGV against `ExecKeyContext` directly and calls `ff_script` / `fcall` by Lua function name. A Postgres backend cannot fulfil these without re-inventing the Lua function names it has to pretend to emit.
2. **`ferriskey::Client` surfacing on `FlowFabricWorker`.** `FlowFabricWorker::client()` returns `&Client` (issue #87). Any consumer using the SDK for something the SDK doesn't cover reaches in and issues raw commands against a concrete Valkey client. The read-surface work sealed this in principle; in practice `Client` is still pub-accessible. Stream free-functions (`read_stream`, `tail_stream`) take a `&Client` directly.
3. **`ferriskey::Error` leaking through `SdkError`.** `SdkError::Valkey(#[from] ferriskey::Error)` and `SdkError::valkey_kind() -> Option<ferriskey::ErrorKind>` expose the backend's error type on the public surface (issue #88). `EngineError` covers the classification layer but callers reaching for `valkey_kind()` still bind to `ferriskey`.
4. **Partition wire types.** `ClaimGrant.partition: String` and a handful of REST DTOs carry `{fp:N}` / `{p:N}` tag strings on the wire (issue #91). Consumers pattern-match on this shape. Any non-Valkey backend has different routing concepts; shipping a stringly-typed Valkey-specific tag over the wire binds the wire to Valkey's cluster model.
5. **Stream cursors.** `XRANGE` / `XREAD` markers (`"-"`, `"+"`, `"$"`, `<ms-seq>`) show up on SDK stream APIs as raw strings (issue #92). Consumers concat them. A Postgres backend has `WAL_LSN` or `txid_snapshot` or integer sequence numbers — different vocabulary entirely.
6. **Completion pubsub.** `ff-engine`'s completion listener subscribes to the literal channel `ff:dag:completions` via ferriskey pubsub (`crates/ff-engine/src/completion_listener.rs`). Consumers that want edge-completion fan-in (cairn-fabric's bridge observer, future stream-tailers) reach into this pubsub channel or re-implement their own tail. Issue #90 files a `CompletionStream` trait to cover this, but it is under-specified without a receiving trait shape to plug into — this RFC provides that shape.
7. **`WorkerConfig` connection primitives.** `WorkerConfig` carries `host: String`, `port: u16`, `tls: bool`, `cluster: bool` — all Valkey connection concepts (issue #93). A Postgres backend has `dsn`, a NATS backend has `urls: Vec<String>`. The config shape pre-commits every consumer to Valkey connection semantics.

### §1.3 Why trait-ify the write surface

Phase 1's snapshot types prove the decoupling pattern works: a consumer that only needs read-side coordination state already has zero Valkey dependency in its build graph. Extending the same discipline to the write surface unlocks three concrete affordances:

* **A Postgres backend becomes implementable as a drop-in crate.** The long-standing ask from operators running managed-Postgres-only environments collapses from "fork FlowFabric and rewrite the Lua" to "add a crate that impls `EngineBackend`." The Postgres design is out of scope for this RFC, but the existence of the trait is the prerequisite.
* **The SDK surface stops leaking ferriskey.** Issues #87 + #88 (ferriskey::Client + ferriskey::Error leaks) both collapse to "consumers interact with `dyn EngineBackend` or a generic `<B: EngineBackend>` parameter; ferriskey surfaces nowhere in the SDK's public API." This is the concrete mechanism #87 + #88 need.
* **Testing becomes driver-agnostic.** Today `ff-test` requires a running Valkey cluster because `ClaimedTask::complete()` issues a real FCALL. A trait lets test harnesses stub the backend. We don't *require* an in-memory test backend as part of this RFC — but the trait makes one trivial to build downstream.

### §1.4 Why now (evidence, not assertion)

The arguments for shipping the trait extraction now rather than later are the same shape as RFC-011's §1.3 "why now" — but with one key difference: this work is **not** user-visible. A trait extraction that preserves Valkey-impl behaviour exactly is invisible to any consumer that doesn't opt-in to the abstraction. That changes the risk calculus.

* **Pre-1.0, zero external SDK users.** Only cairn-fabric consumes `ff-sdk`; it is mid-migration against a parallel refactor. No external version promise has been made. Moving from `ClaimedTask::complete(self, payload)` (inherent impl) to `EngineBackend::complete(&self, handle, payload)` (trait method) is the window to rearrange surfaces without a compat shim.
* **Phase 1 sealed the shape of the decoupling contract.** `ExecutionSnapshot` / `FlowSnapshot` / `EdgeSnapshot` + `EngineError` are the template. The write-surface trait slots into the already-established pattern; we are not designing a new abstraction from scratch, we are extending a working one.
* **Call-site debt compounds.** Every new FCALL-wrapping SDK method added to `ClaimedTask` is a future trait-method to retrofit. Worker F's audit counted 18; a Phase-3-era `suspend_with_reason` RFC could push that to 20. The structural cost per new method on the inherent impl is lower than on a trait definition; the cost to retrofit later scales linearly with the method count.
* **Cairn's parallel migration absorbs one alignment.** Cairn is actively decoupling its own internals. Their alignment PR slot can take one well-scoped shape change; the trait extraction is one concrete such change. Deferring means a second alignment cycle later.

### §1.5 Scope and timing (owner decisions, locked)

Four constraints bound this RFC before design starts. Each is an explicit owner decision and is not up for re-litigation in the debate rounds:

1. **Higher-op granularity, not FCALL-mirror.** The trait exposes ~10-12 business operations, not 18 thin wrappers around the current Lua function names. The point is to give a Postgres backend design freedom to model write transactions as it sees fit — a single `complete` method can run one Lua FCALL on Valkey and a single `UPDATE ... RETURNING` on Postgres, but only if the trait method is named after the business operation (`complete`) rather than the Valkey mechanism (`ff_complete_execution`). See §3 for the inventory.
2. **Consumer migration is not a gate.** Cairn migrates on its own timeline. This RFC authorises the trait shape; consumer migration (cairn, future external consumers) is downstream and non-blocking. See RFC-011's §7.4 for the precedent — cairn's phase-4 realignment landed after phase 3 merged, and the trait extraction follows the same pattern.
3. **Release 0.3.0 ships before this trait lands.** 0.3.0 waits for the cairn asks already in flight (cairn-blocking work, `ff-sdk` gaps) and is not gated on the trait. The trait lands in 0.4.x as a cluster of PRs per the follow-up issues. This RFC is the architectural keystone; implementation lands post-0.3.0.
4. **OTEL via the existing ferriskey-adjacent dep, feature-gated.** FlowFabric already carries an optional OTEL dep; the trait does not introduce a new metrics framework. Per-method instrumentation lives behind the existing feature flag, unchanged. Observability is not in this RFC's scope beyond noting that the trait dispatch must preserve current span structures (§7.4).

---

## §2 Non-goals

This RFC defines a trait shape. It is explicitly not:

* **Not a Postgres backend implementation.** A trait proposal is necessary-but-not-sufficient for a Postgres backend. The Postgres crate is a separate project with its own RFC (`RFC-014-postgres-backend`, notional) and its own review cycle. This RFC's §5 names which methods a Postgres impl must fulfil; it does not specify *how* Postgres fulfils them.
* **Not a cairn migration.** Consumer migration is their concern and their timeline. Cross-team coordination is not on the critical path.
* **Not a re-design of the execution model.** The trait mirrors the existing semantics — claim / progress / complete / fail / cancel / suspend / delay / wait_children / resume — just typed. RFCs 001-009 define those semantics; this RFC adds a dispatch seam, not a new semantics layer. A reviewer who finds a semantic mismatch between the trait and the existing RFCs should flag a bug in the trait, not propose a semantics change.
* **Not a replacement of FCALLs inside the Valkey impl.** The 18 Lua functions continue to exist. They are how the Valkey backend fulfils the trait. A single trait method may drive one or several FCALLs; the point is that the FCALL names and KEYS/ARGV layouts are backend-internal once the trait lands. They do not appear in the public SDK surface.
* **Not a new admin API surface.** Admin operations (waitpoint HMAC rotation, partition collision diagnostics, cutover runbook tooling) live on a separate trait or sit on the backend impl as inherent methods. §3.2 names the ones that *don't* go on `EngineBackend`.
* **Not a synchronous-API proposal.** The trait is `async` throughout. The existing SDK is async; a blocking variant (if ever wanted) is a follow-up trait, not a parallel definition here.
* **Not a multi-tenancy / capability-routing redesign.** RFC-009's capability routing sits above the trait. `claim` takes a capability set; how the backend satisfies routing is backend-internal. Changes to the routing model itself belong in a follow-up RFC.

---

## §3 Proposed `EngineBackend` trait shape

### §3.1 Operation inventory (16 methods — round-5 + round-7 amended)

The owner's decision pins the granularity target at ~10-12 business-operation methods; round-2 revisions (K#2 split of `resume`, K#6 split of `progress`) brought the count to 13; round-5's amendment added the two missing lease-releasing peers of `suspend` (`delay`, `wait_children`) for a count of 15; round-7's amendment adds one gap-fill method (`create_waitpoint`, §R7) for a final count of 16. The owner's "~10-12" remains a range, not a hard ceiling; 16 is within the spirit of the decision — splits-for-honesty moves dominate (the `progress`/`append_frame` split replaces a fused method whose atomicity was unsound; `observe_signals`/`claim_from_reclaim` replaces one method whose multi-round-trip honesty was broken; `delay` and `wait_children` elevate two call sites that are structural peers of `suspend`), with `create_waitpoint` the one gap-fill add (the trait had no slot for pending-waitpoint issuance; see §R7.2.2). The `async fn` compile-count is 17 (one higher than the method count because method 3/3b are two `async fn`s under one taxonomy slot). Below is the round-5 + round-7 inventory.

The inventory assumes one trait (not multiple domain-split traits) per §6.2; a consumer holding a `Backend: EngineBackend` gets the full write surface without composing four traits.

#### §3.1.1 Claim + lifecycle ops

**1. `claim(lane, capabilities, policy) -> Result<Option<Handle>, EngineError>`.** Fresh-work claim only. Combines today's scheduler grant issuance (`ff_issue_claim_grant`) and the execution claim proper (`ff_claim_execution`). Internally, the Valkey impl runs both FCALLs — possibly in a pipeline if the scheduler has already pre-staged a grant — and returns an opaque `Handle`. The handle carries whatever state the backend needs to subsequently operate on the claim (lease token, exec id, attempt id, capability binding); to the caller it is a concrete opaque cookie (§4.1). Postgres impl: single `SELECT ... FOR UPDATE SKIP LOCKED` + `INSERT INTO leases` in one transaction.

*Rationale for combining grant + fresh claim:* from the worker's standpoint "I got some work" is one event. The scheduler-vs-worker split in today's code is an internal routing optimisation (grants live in a separate partition so scheduling iteration is bounded); it is not a user-visible state transition. Backends that don't split routing (Postgres has no shard concept at this level) collapse the two. Split trait methods would leak the Valkey-specific pre-staging model into the contract. See §7.1 for the debate on whether to instead split the two.

*Round-2 note (K#3):* `claim` covers only the fresh-work path. Today's code has three distinct entry points — `claim_from_grant` (fresh), `claim_from_reclaim_grant` (resumed after crash), `claim_via_server` (HTTP-routed through scheduler). Round-1 fused all three into one method, which erased the wire-distinct `ReclaimGrant` vs `ClaimGrant` type split and silently changed crash-recovery semantics. Round-2 resolution: `claim` is fresh only; reclaim-grant consumption moves to method 9 (`claim_from_reclaim`, below); `claim_via_server` is explicitly NOT on the trait — it is an HTTP orchestration concern living on the server/scheduler crate above the trait. See §7.1 for the expanded debate.

*Crash-recovery story for `claim`:* because fresh-claim internally runs grant-issue (FCALL-A) then execution-claim (FCALL-B), a worker crash between A and B leaves an issued grant with a TTL. The reclaim scanner's existing timeout path reaps expired grants. If FCALL-A succeeds and FCALL-B fails, `claim` returns `Err(EngineError::Contention(...))` or `Err(EngineError::Transport(...))` and the grant is re-reaped naturally. There is no externally-observable partial state.

*Maps to Valkey FCALLs:* `ff_issue_claim_grant` (if not pre-staged) + `ff_claim_execution`. Reclaim (`ff_claim_resumed_execution`) is NOT reachable through this method.

**2. `renew(handle) -> Result<LeaseRenewal, BackendError>`.** Lease renewal. `LeaseRenewal` carries the new expires-at timestamp (monotonic on Valkey via `now_ms`; coordinator clock on Postgres). If the lease has been stolen — fence-triple mismatch — the backend returns a typed error; the caller terminates the attempt. The handle is *not* consumed; renewal is a read-like mutation.

*Maps to:* `ff_renew_lease`.

**3. `progress(handle, percent, message) -> Result<(), EngineError>`.** Progress / heartbeat for numeric-progress updates. Maps directly to one backend round-trip (Valkey: one FCALL `ff_update_progress`; Postgres: one UPDATE).

**3b. `append_frame(handle, frame) -> Result<(), EngineError>`.** Frame append for stream-based status APIs. Separate method because (a) it has a distinct backend hot path (`ff_append_frame` FCALL on Valkey; INSERT on Postgres), (b) many consumers only ever call one or the other, and (c) fusing forced a `Both` variant whose atomicity was ambiguous (round-1 §7.5 / K#6).

*Round-2 note (K#6):* round-1 merged these into `progress(update: ProgressUpdate)` with variants `{ Percent, Frame, Both }`. The `Both` variant had no honest cost model: two FCALLs violated §3.4 atomicity, and a new combined Lua function was unbudgeted. The split is the correct answer. The trait grows from 12 to 13 methods; the ergonomic cost is one extra method name, and the honesty cost is zero. §7.5 updated accordingly.

*Maps to:* `ff_update_progress` (method 3) and `ff_append_frame` (method 3b).

**4. `complete(&handle, payload) -> Result<(), BackendError>`.** Terminal success. Round-4 (M-D2): borrows the handle rather than consuming it, so callers retain the handle for retry under `EngineError::Transport`. Backend enforces state-transition idempotency (§3.4 idempotent replay): a second call with the same handle after a successful first returns "already terminal" (`EngineError::State`). Payload is the attempt's result bytes (caller-chosen encoding; trait doesn't mandate JSON). On Valkey, complete runs a single 12-KEY FCALL that mutates exec state, flow-membership terminal set, unblocks children, and publishes a completion event. On Postgres, a single transaction mutates `executions`, `flow_memberships`, `child_dependencies`, and inserts into `completion_events`.

*Maps to:* `ff_complete_execution`.

**5. `fail(handle, reason, classification) -> Result<FailOutcome, BackendError>`.** Terminal failure (possibly with retry scheduling). `FailOutcome` carries whether the failure triggered a retry (with the new attempt's delay) or was final. The retry-decision policy is backend-internal — retry table, budget admission, and delay calculation all live inside the FCALL on Valkey and the transaction on Postgres. Caller sees the outcome.

*Maps to:* `ff_fail_execution`. Includes the retry-scheduling sub-branches today visible as Lua status codes.

**6. `cancel(&handle, reason) -> Result<(), BackendError>`.** Terminal cancellation. Round-4 (M-D2): borrows. Reason is an opaque caller string for observability; trait does not constrain its vocabulary. Idempotent replay: second call after success returns `EngineError::State`.

*Maps to:* `ff_cancel_execution`.

**7. `suspend(handle, waitpoints, timeout) -> Result<Handle, EngineError>`.** Lease-releasing suspension. The attempt hands the lease back and waits for any of a set of waitpoints to fire, or for the timeout. Round-2 change: returns a fresh `Handle` with internal `kind = Suspended` (§4.1), not a distinct `SuspendToken` associated type. Waitpoints are a typed `Vec<WaitpointSpec>` carrying HMAC-signed tokens generated by the backend. Timeout is optional.

*Maps to:* `ff_suspend_execution` + `ff_create_pending_waitpoint`.

**8. `observe_signals(handle) -> Result<Vec<ResumeSignal>, EngineError>`.** Observation-only: returns every signal that has fired for the handle's suspended attempt. Pure read-like op. On Valkey: HGETALL + per-waitpoint HGET/HMGET over matcher slots (exactly today's `ClaimedTask::resume_signals` path, `task.rs:1156`). On Postgres: a single `SELECT`. Does NOT claim anything, does NOT consume a grant, and does NOT evaluate reclaim-grant predicates.

**9. `claim_from_reclaim(token) -> Result<Option<Handle>, EngineError>`.** Grant-consumption: the reclaim scanner (out-of-band) issues a `ReclaimGrant`; this method turns the grant into a claimed `Handle` for the resumed attempt. Consumes the grant. Returns `None` if the grant has expired or was already consumed. Maps to `ff_claim_resumed_execution` on Valkey.

*Round-2 note (K#2 + K#3):* round-1 had a single `resume(signals) -> Option<ResumeHandle>` method that fused three atomicity domains: signal observation (pure read), predicate evaluation over matcher slots (client-side), and grant consumption (one FCALL). That is three round-trips on Valkey by physical necessity; any consumer reading the method signature would model it as one op. The §3.4 atomicity contract ("per-op state transitions are atomic — partial state is not observable") was false for `resume` as drafted.

Split resolution:
* `observe_signals` is pure observation. Multi-round-trip internally on Valkey is fine because no state mutates.
* `claim_from_reclaim` is single-op grant-consumption with §3.4 atomicity preserved.
* The predicate evaluation that sits between them is a consumer-side / scanner-side concern, not a trait responsibility. The reclaim scanner is what runs predicates and issues grants; the trait exposes the grant-consumption endpoint the scanner targets.

This also addresses K#3's reclaim-third-path concern: `claim` is fresh, `claim_from_reclaim` is reclaim-resumed, and `claim_via_server` (HTTP-routed) is not on the trait at all — it lives on the server crate above the trait.

*Maps to:* (8) read-path (HGETALL/HMGET today) — no state mutation. (9) `ff_claim_resumed_execution`.

**14. `delay(handle, delay_until) -> Result<(), EngineError>`.** Lease-releasing delay. The attempt hands the lease back and the execution moves to the `delayed` state; a scheduler tick at or after `delay_until` re-activates it. Structural peer of `suspend`: single FCALL under fence triple, non-terminal. Unlike `suspend`, `delay` waits on wall-clock time rather than a waitpoint set, and does NOT return a fresh `Handle` — the caller's handle is released outright; re-entry happens via a fresh `claim` after the scheduler fires.

*Maps to:* `ff_delay_execution`. Call site: `ClaimedTask::delay_execution` (`crates/ff-sdk/src/task.rs:414`).

*Round-5 note:* the round-4 inventory omitted this method. It is a structural peer of `suspend` — KEYS/ARGV shape matches (9 KEYS, 5 ARGV; fence triple; attempt-timeout zset membership), Lua function is one-to-one, and the state transition is lease-releasing-non-terminal. Omitting it would have forced Stage 1 to ship an incomplete trait.

**15. `wait_children(handle) -> Result<(), EngineError>`.** Lease-releasing move to `waiting_children`. The attempt hands the lease back and the execution waits for its child-dependency set to complete; a completion event on the last child re-activates it. Structural peer of `suspend` and `delay`: single FCALL under fence triple, non-terminal. Waitpoint kind here is "all children terminal"; evaluation is scheduler-side, not matcher-slot-side.

*Maps to:* `ff_move_to_waiting_children`. Call site: `ClaimedTask::move_to_waiting_children` (`crates/ff-sdk/src/task.rs:460`).

*Round-5 note:* as with method 14, round-4 omitted this. It is a structural peer of `suspend` (9 KEYS, 4 ARGV; fence triple; attempt-timeout zset membership). Included in the amendment for the same reason.

**16. `create_waitpoint(handle, waitpoint_key, expires_in) -> Result<PendingWaitpoint, EngineError>`.** Issues a pending waitpoint for future signal delivery. Waitpoints have two wire states — **pending** (token issued, not yet backing a suspension) and **active** (bound to a suspension). This method creates a waitpoint in the pending state; a later `suspend` transitions pending → active via the Lua `use_pending_waitpoint` ARGV flag (`flowfabric.lua:3603,3641,3690`). If buffered signals already satisfy the condition, `suspend` returns `AlreadySatisfied` and the waitpoint activates without the lease being released. Pending-waitpoint expiry is a first-class terminal error on the wire (`PendingWaitpointExpired` at `ff-script/src/error.rs:170,403-408`).

*Returns a `PendingWaitpoint { waitpoint_id, hmac_token }`*, where `hmac_token: WaitpointHmac` reuses the existing `ff-core::backend::WaitpointHmac` (Debug-redacts; no Display impl).

*Maps to:* `ff_create_pending_waitpoint`. Call site: `ClaimedTask::create_pending_waitpoint` (`crates/ff-sdk/src/task.rs:686`).

*Round-7 note:* added by the round-7 amendment (issue #117). Placed in §3.1.1 alongside other lifecycle ops (no new sub-bucket); the trait does not grow a "waitpoint management" §3.1.4 bucket for one method. Naming kept as `create_waitpoint` (not `create_pending_waitpoint`) — the trait surface describes the caller's business op, with the pending-vs-active disambiguation in the method's rustdoc. See §R7.2.2 for the full rationale and §R7.6.3 for the naming discussion. The `suspend`-side migration (return type widen + input-shape rework) is explicitly NOT part of round-7; it defers to Stage 1d per §R7.1 / §R7.6.1.

#### §3.1.2 Read-path ops (leveraging Phase 1)

**10. `describe_execution(id) -> Result<Option<ExecutionSnapshot>, EngineError>`.** Uses `ff_core::contracts::ExecutionSnapshot` directly; no reshaping. Already shipped by Phase 1 as an inherent method; promoted to trait here to round out the read surface.

*Maps to:* the read path sealed by issue #58.1.

**11. `describe_flow(id) -> Result<Option<FlowSnapshot>, EngineError>`.** Symmetric with method 10.

*Maps to:* issue #58.2.

**12. `cancel_flow(id, policy, wait) -> Result<CancelFlowResult, EngineError>`.** Flow-level cancel. `policy` enumerates cascade semantics (cancel-all, cancel-pending-only, etc.; already defined in `ff_core::contracts`). `wait` is whether to block for termination. Returns the set of (id, outcome) pairs — already shipped by RFC-011-adjacent work.

*Scope note (K's "deserves debate" #2):* `cancel_flow` is admin-shaped (control-plane op, no claim held by the caller). K asked whether this should live on `AdminBackend` rather than `EngineBackend`. Round-2 defence: `cancel_flow` maps to a single backend round-trip (one FCALL on Valkey, one transaction on Postgres) and shares the atomicity / fence / error-classification contract of the rest of the trait. `AdminBackend` is where ops with a categorically different consumer class live (operator tools, key rotation); `cancel_flow` is callable from any workflow code that orchestrates sub-flows. Keeping it on `EngineBackend` avoids a second trait for consumers who already hold one. Recorded as a defensible position in §8.17; reviewers who disagree should raise it in round 3.

*Maps to:* `ff_cancel_flow` + the server-side orchestration around it.

#### §3.1.3 Out-of-band ops

**13. `report_usage(handle, budget, dimensions) -> Result<AdmissionDecision, EngineError>`.** Budget reporting with optional admission gating. Today, budget reporting is physically embedded on the hot path of `complete`/`fail` via the `ff_report_usage_and_check` Lua wrapper and a report-check ARGV bundle. The trait splits them: `report_usage` runs in its own op-slot so the complete-path doesn't grow with budget feature scope.

*Rationale for splitting out:* budget reporting has its own failure modes (admission failure, quota exhaustion) that today require threading through the complete/fail paths' result types. By giving it its own method, `complete`'s signature stays clean and `fail`'s stays focused on retry scheduling. The hot-path cost is one additional FCALL per attempt for budget-using workloads; that overhead is already paid today (the report-check path makes the same FCALL), just hidden inside the complete wrapper.

*Maps to:* `ff_report_usage_and_check`.

### §3.2 Explicitly not in the trait (with justifications)

Four categories of write-like operation are omitted from `EngineBackend` deliberately. Each justification names the alternative surface.

**Tag reads and writes.** Today `ExecutionSnapshot.tags` carries the tag map. Writes to tags happen at create-time (on `ff_create_execution`) and nowhere else in the SDK-level API. Additional tag-mutation ops (if ever wanted) fold into `describe_execution` for reads and a small `set_tags` op on a separate `AdminBackend` trait. Not on the core path; not on this trait.

**Waitpoint HMAC rotation.** Admin-only operation, run by operators during key rotation, not by worker code. Lives on a parallel `AdminBackend` trait (`rotate_waitpoint_hmac`, `issue_hmac_rotation_grace_period`). A worker with an `EngineBackend` handle has no business rotating keys. Separating admin concerns keeps the hot-path trait lean.

**Cross-execution signal observation (round-4, M-D5).** `observe_signals(&Handle)` on `EngineBackend` requires a handle — only the current handle-holder reads that attempt's signals. Admin / ops use cases that want to observe signals for an arbitrary execution id (without holding a handle; e.g., cairn's bridge-observer introspection) live on `AdminBackend::describe_signals(execution_id)`. This is symmetric with the admin-describe methods; `EngineBackend` is worker-scoped, `AdminBackend` is operator-scoped.

**Stream reads (XRANGE / XREAD).** Stream access is a related-but-distinct decoupling issue (#92). Stream cursors are a different abstraction (they need a cursor type; XRANGE markers don't map to row-based backends without a translation layer). A separate `StreamBackend` trait owns the stream surface. Callers who want both hold two trait objects or a composite. This RFC names StreamBackend's existence; its shape lives in the #92 follow-up RFC.

**Completion pubsub subscription.** Issue #90 files a `CompletionStream` trait. This RFC folds the trait's return-types into `EngineBackend`'s shape — specifically, any trait method that today returns a "subscribe to completions for this entity" side-effect returns a `CompletionStream` typed by the associated type (§4). The stream type itself is this RFC's responsibility (it shapes return types); the subscription mechanism for bulk-tailing the completion channel is #90's trait.

### §3.3.0 Type inventory for the trait signatures (round-4, per M3; round-7 addendum)

The §3.3 signatures reference 17 supporting types. Honesty requires naming which exist today and which are Stage-0 deliverables. Round-7 amendment adds two type-moves/introductions (`AppendFrameOutcome`, `PendingWaitpoint`), marks `AdmissionDecision` for removal, and adds `#[non_exhaustive]` to `ReportUsageResult`. See §R7.2 for rationale.

**Exists in-tree today (no change needed):**

| Type | Crate / file | Notes |
|------|--------------|-------|
| `LaneId` | `ff-core::types` (`crates/ff-core/src/types.rs:437`) | Newtype over `String`. |
| `FailOutcome` | `ff-sdk::task` (`crates/ff-sdk/src/task.rs:169`) | Stays in ff-sdk OR moves to ff-core::contracts as part of Stage 0 cleanup (re-export via `pub use` at old path). Decision at Stage-0 impl time. |
| `CancelFlowResult` | `ff-core::contracts` (`crates/ff-core/src/contracts.rs:1241`) | Use as-is. |
| `ExecutionId` / `FlowId` / `EdgeId` | `ff-core::types` | Use as-is. |
| `ExecutionSnapshot` / `FlowSnapshot` | `ff-core::contracts` | Phase 1 landed. |
| `ResumeSignal` | `ff-sdk::task` (`crates/ff-sdk/src/task.rs:137`) | **Stage 0 MOVE** to `ff-core::contracts`; `pub use` shim at old path through 0.4.x. |

**Stage 0 new-type deliverables (Phase-1-scope, additive):** 13 new public types, each a ~10-30 line addition in `ff-core::contracts` (unless noted). None are complex; most are small structs/enums mirroring what today lives as ad-hoc strings or `Vec<String>` on the inherent impl.

| Type | Target crate | Approximate shape | Notes |
|------|--------------|-------------------|-------|
| `CapabilitySet` | `ff-core::contracts` | Newtype over `Vec<String>` (today: `Vec<String>` on `WorkerConfig`). | §7.2 debate still open re bitfield; newtype is the round-2 lean. |
| `ClaimPolicy` | `ff-core::contracts` | Enum or struct carrying fairness hints, timeout, retry count. Start minimal: `ClaimPolicy { max_wait: Option<Duration> }`. | Bikeshed-prone; keep minimal at Stage 0. |
| `Frame` | `ff-core::contracts` | Struct: `{ bytes: Bytes, kind: FrameKind, seq: Option<u64> }`. | Already conceptually exists in the `ff_append_frame` FCALL's args. |
| `WaitpointSpec` | `ff-core::contracts` | `{ kind: WaitpointKind, matcher: Bytes, hmac_token: WaitpointHmac }`. | HMAC token shape mirrors today's signed-waitpoint wire. |
| `FailureReason` | `ff-core::contracts` | Struct: `{ message: String, detail: Option<Bytes> }`. | Today: ad-hoc string arg. |
| `FailureClass` | `ff-core::contracts` | Enum: `{ Transient, Permanent, InfraCrash, Timeout, Cancelled }`. | Mirrors Lua-side classification codes. |
| `UsageDimensions` | `ff-core::contracts` | Struct mirroring `ff_report_usage_and_check`'s ARGV: token-counts, wall-time, custom-dim map. | |
| `BudgetId` | `ff-core::types` | Newtype over `String` (matches `LaneId` / `FlowId` discipline). | **Erratum (Stage 1a):** already lives in `ff-core::types` via the `cf_id!` macro (`crates/ff-core/src/types.rs:278`); no new-type work needed. This row is a no-op for Stage 0 / 1a. |
| `ReclaimToken` | `ff-core::contracts` | Newtype wrapping whatever bytes the reclaim scanner issues (today a `ReclaimGrant` struct in `ff-core::contracts:161`). Likely `ReclaimToken(ReclaimGrant)`. | May be pure re-export if `ReclaimGrant` proves sufficient. |
| `LeaseRenewal` | `ff-core::contracts` | `{ expires_at_ms: u64, lease_epoch: u64 }`. | |
| `AdmissionDecision` | `ff-core::contracts` | Enum: `{ Admitted, Throttled { retry_after_ms: u64 }, Rejected { reason } }`. | Returned by `report_usage` at round-4; **REMOVED by round-7** — replaced by `ReportUsageResult` (see §R7.2.3). |
| `CancelFlowPolicy` | `ff-core::contracts` | Today this shape lives inside `CancelFlowArgs` (`contracts.rs:1233`) — extract the policy enum as a named type; keep `CancelFlowArgs` as the REST wrapper. | |
| `CancelFlowWait` | `ff-core::contracts` | Enum: `{ NoWait, WaitTimeout(Duration), WaitIndefinite }`. | |
| `CompletionPayload` | `ff-core::contracts` | Struct: `{ execution_id, outcome, payload_bytes, produced_at_ms, … }`. | Also deliverable from issue #90. |

**Round-7 addendum — type deltas for the #117 deferrals amendment (Stage-0-style, landing in the same PR as the trait changes).**

| Type | Target crate | Round-7 change | Notes |
|------|--------------|----------------|-------|
| `AppendFrameOutcome` | `ff-core::backend` | **Stage-0-style MOVE** from `ff-sdk::task` (`task.rs:137-143`). Derives widen from `Clone, Debug` to `Clone, Debug, PartialEq, Eq` matching `FailOutcome` precedent. `pub use ff_core::backend::AppendFrameOutcome` shim at old path through 0.4.x. | Shape: `{ stream_id: String, frame_count: u64 }`. No `#[non_exhaustive]` — construction is internal to `ff-sdk`; no external constructors anticipated (consumer-shape evidence, per §R7.2.1 / MN3). `stream_id: String` is a stable shape commitment — a future typed `StreamId` newtype would be its own breaking change (§R7.5.6 / MD2). |
| `PendingWaitpoint` | `ff-core::backend` | **NEW type** for `create_waitpoint`'s return. | Shape: `{ waitpoint_id: WaitpointId, hmac_token: WaitpointHmac }`. `#[non_exhaustive]` (new type; preserves additivity). `WaitpointHmac` already exists at `crates/ff-core/src/backend.rs:247-277` (Debug-redacts; no Display impl). |
| `ReportUsageResult` | `ff-core::contracts` | **Gains `#[non_exhaustive]`** at `contracts.rs:1400`. Becomes the new return type of `report_usage` (replacing `AdmissionDecision`). Both deltas land in one atomic commit — no semver-two-step (§R7.7 / M3). | Existing variants: `Ok | AlreadyApplied | SoftBreach { … } | HardBreach { … }` (`contracts.rs:1400-1418`). Wire-fed by Lua `ff_report_usage_and_check` (parser at `task.rs:1194-1213`). |
| `AdmissionDecision` | `ff-core::contracts` | **REMOVED.** | In-tree references unwind together: type def (`backend.rs:367-376`), trait sig (`engine_backend.rs:192-197`), stub impl (`ff-backend-valkey/src/lib.rs:1240-1249`), three test constructions (`backend.rs:888-890`). See §R7.2.3. |

**Correction to §5.1's landing gate.** Round-2 said Stage 1 is "no public-surface changes beyond additive." That claim conflates trait-extraction with type-plumbing. Round-4 split:

* **Stage 0** is where the ~13 new types + `EngineError::Unavailable` variant + `Transport` broadening + `ResumeSignal` crate move land. All additive; the crate move has a `pub use` shim. This stage is public-surface-touching by construction.
* **Stage 1** is pure trait extraction against the Stage-0 types + the existing SDK wrapper types. This stage is "no public-surface change beyond additive" in the strict sense: the trait itself is new-public, every other public surface is stable.

The distinction is crisp and matches how the landing PRs should be reviewed.

### §3.3 Trait method signatures (round-2 revised — option C, concrete types)

Each method has a Rust signature sketch below. Types use `ff_core::contracts` vocabulary where available. The sketch is illustrative; final signatures land on the implementation PRs per-method.

> **RFC-only note:** the signatures are Rust-prose for debate purposes. They are not compiled. The debate rounds should treat them as semantic proposals, not compilable interfaces.

**Round-2 design choice (K#5 resolution, §7.7 closed).** Round-1 used five associated types (`Handle`, `ResumeHandle`, `SuspendToken`, `Error`, `CompletionStream`) AND claimed the trait should be `dyn`-safe. Those are incompatible in practice: a `dyn EngineBackend` trait object cannot usefully name `<dyn EngineBackend as EngineBackend>::Handle` at a call site. K#5 forced the resolution into this RFC (it is not a follow-up question).

The round-2 trait uses **concrete types at the boundary**:

* `Handle` is a single concrete opaque struct. It wraps a backend-tag + an inline byte buffer for backend-specific state. One `Handle` covers fresh-claim, reclaim-resume, and suspended-then-resumed cases; the kind is an internal discriminator checked by the backend on every op. The "accidentally pass a suspended handle to a complete path" class of bug becomes a runtime check (`EngineError::State(...)` returned) rather than a compile-time reject. Defended in §8.4 (revised): the compile-time type split was costing us object-safety and generics-proliferation across SDK wrappers; the runtime check is cheap and matches today's fence-triple check model.
* `Error` is the concrete `EngineError` from `crates/ff-sdk/src/engine_error.rs` (Phase 1 landed). Backends that need to carry a backend-private transport error (e.g., `ferriskey::Error`) do so inside their `EngineError::Transport(Box<dyn Error + Send + Sync>)` variant.
* `CompletionStream` becomes `Pin<Box<dyn Stream<Item = CompletionPayload> + Send>>` — erased at the trait boundary. Per-backend optimization (zero-alloc streams) is a backend-internal concern below the Box; the Box overhead is once-per-subscription, not per-item.
* `SuspendToken` and `ResumeHandle` collapse. `suspend` returns a fresh `Handle` (same concrete type, internal kind = Suspended); `claim_from_reclaim` returns a fresh `Handle` (internal kind = ResumedActive). The concrete-struct decision makes the type distinction moot.

```text
pub struct Handle {
    backend: BackendTag,      // { Valkey, Postgres, ... } for runtime dispatch within the handle
    kind: HandleKind,         // { Fresh, Resumed, Suspended } — #[non_exhaustive], checked on every op call
    opaque: HandleOpaque,     // newtype over Box<[u8]>; backend-private state
}

#[async_trait]
pub trait EngineBackend: Send + Sync + 'static {
    async fn claim(
        &self,
        lane: &LaneId,
        capabilities: &CapabilitySet,
        policy: ClaimPolicy,
    ) -> Result<Option<Handle>, EngineError>;

    async fn renew(
        &self,
        handle: &Handle,
    ) -> Result<LeaseRenewal, EngineError>;

    async fn progress(
        &self,
        handle: &Handle,
        percent: Option<u8>,
        message: Option<String>,
    ) -> Result<(), EngineError>;

    async fn append_frame(
        &self,
        handle: &Handle,
        frame: Frame,
    ) -> Result<AppendFrameOutcome, EngineError>; // round-7: widened from ()

    async fn complete(
        &self,
        handle: &Handle,                 // round-4 (M-D2): borrow, not consume
        payload: Option<Bytes>,
    ) -> Result<(), EngineError>;

    async fn fail(
        &self,
        handle: &Handle,                 // round-4 (M-D2)
        reason: FailureReason,
        classification: FailureClass,
    ) -> Result<FailOutcome, EngineError>;

    async fn cancel(
        &self,
        handle: &Handle,                 // round-4 (M-D2)
        reason: &str,
    ) -> Result<(), EngineError>;

    async fn suspend(
        &self,
        handle: &Handle,                 // round-4 (M-D2): suspend also borrows;
        waitpoints: Vec<WaitpointSpec>,  // the returned fresh Handle (kind=Suspended)
        timeout: Option<Duration>,       // is a new cookie the caller replaces their old one with.
    ) -> Result<Handle, EngineError>;

    async fn observe_signals(
        &self,
        handle: &Handle,
    ) -> Result<Vec<ResumeSignal>, EngineError>;

    async fn claim_from_reclaim(
        &self,
        token: ReclaimToken,
    ) -> Result<Option<Handle>, EngineError>; // kind=Resumed on Some

    // Round-5 amendment: lease-releasing peers of `suspend`.
    async fn delay(
        &self,
        handle: &Handle,
        delay_until: TimestampMs,
    ) -> Result<(), EngineError>;

    async fn wait_children(
        &self,
        handle: &Handle,
    ) -> Result<(), EngineError>;

    async fn describe_execution(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<ExecutionSnapshot>, EngineError>;

    async fn describe_flow(
        &self,
        id: &FlowId,
    ) -> Result<Option<FlowSnapshot>, EngineError>;

    async fn cancel_flow(
        &self,
        id: &FlowId,
        policy: CancelFlowPolicy,
        wait: CancelFlowWait,
    ) -> Result<CancelFlowResult, EngineError>;

    async fn report_usage(
        &self,
        handle: &Handle,
        budget: &BudgetId,
        dimensions: UsageDimensions,
    ) -> Result<ReportUsageResult, EngineError>; // round-7: replaces AdmissionDecision

    // Round-7 amendment: pending-waitpoint issuance (gap-fill for SDK
    // ClaimedTask::create_pending_waitpoint). See §R7.2.2.
    async fn create_waitpoint(
        &self,
        handle: &Handle,
        waitpoint_key: &str,
        expires_in: Duration,
    ) -> Result<PendingWaitpoint, EngineError>;
}
```

The trait is both object-safe (`dyn EngineBackend` works; no associated types to name) and usable with generics (`<B: EngineBackend>`). Both dispatch styles are available to consumers without trait-level reshape. `#[async_trait]` is required for `dyn`-safety on async methods pre-Rust-2024-trait-dyn-async (§7.15 below addresses this choice).

### §3.4 Safety properties preserved

Every trait method must preserve the same fence / idempotency / atomicity properties the current inherent implementation enforces. The trait contract names them; the Valkey backend's current behaviour fulfils them by construction (the Lua functions are already written that way); a future Postgres backend must also fulfil them.

**Fence triple**: every op that mutates attempt state requires `(lease_id, lease_epoch, attempt_id)`. The handle carries these internally; the backend enforces them on every op call. On Valkey, fence-triple checks happen inside the Lua function via keys/args. On Postgres, fence-triple checks happen via `WHERE lease_id = $1 AND lease_epoch = $2 AND attempt_id = $3` in the UPDATE clause.

**Idempotent replay**: `complete`, `fail`, `cancel`, `suspend`, `delay`, `wait_children` are idempotent under replay — calling twice with the same handle and args returns the same result (success on first call, "already in that state" on subsequent calls — terminal for `complete`/`fail`/`cancel`; lease-released-non-terminal for `suspend`/`delay`/`wait_children`, where the second call returns `EngineError::State` because the fence triple no longer matches a live lease). The trait contract mandates this. Callers can safely retry under network uncertainty.

**Atomicity**: per-op state transitions are atomic — partial state is not observable. On Valkey this is the single-FCALL-per-op property (RFC-011 §5.5 closed the last cross-partition gap). On Postgres this is the per-transaction property. The trait contract names the atomicity requirement; a backend that can't honour it (e.g., a NATS-backed design that can't promise cross-subject atomicity) either must not implement `EngineBackend` or must degrade specific ops with a documented contract deviation.

*Exception:* `observe_signals` (method 8) explicitly does NOT offer atomicity — it is a pure read with potentially multiple round-trips on Valkey (HGETALL + per-waitpoint HMGET). Because no state is mutated, atomicity is not semantically meaningful. Listed here for completeness.

**Monotonic lease epoch**: lease epoch only advances. No op moves it backward. The backend enforces; the trait contract names the property for consumer-side correctness proofs.

### §3.4.1 Commit atomicity vs. notification atomicity (round-2, K#4)

Round-1 §3.4 asserted "per-op state transitions are atomic" without distinguishing the state-transition commit from downstream side-effect delivery. For single-entity ops (e.g., `renew`, `progress`), the distinction is immaterial. For cross-entity ops (`complete`, `fail`, `cancel`, `cancel_flow`, `suspend`, `delay`, `wait_children`, `claim_from_reclaim`), the distinction is load-bearing. `complete` in particular mutates `executions`, flow-membership terminal set, child-dependency edges, and publishes a completion event. Valkey covers all four within one FCALL; Postgres cannot cover the NOTIFY publication within the row-commit transaction (NOTIFY delivers post-commit).

Round-2 contract distinguishes two atomicity classes:

**Commit atomicity (required of every backend that impls `EngineBackend`).** The backend's persistent state transition is atomic with respect to itself. Either the complete/fail/cancel/cancel_flow transition is fully committed or it is not observable. On Valkey: one FCALL. On Postgres: one transaction spanning all four entity tables. On any other backend: equivalent per-op single-transaction semantics. A backend that cannot guarantee commit atomicity for these cross-entity ops does NOT implement `EngineBackend`; it either composes partial backends or ships with an explicit contract deviation.

**Notification atomicity (NOT guaranteed across backends).** Downstream subscribers to completion events observe the event in their own time. Valkey gives near-atomicity (pubsub is best-effort in-process; subscribers receive shortly after commit). Postgres gives notify-after-commit (NOTIFY fires at `COMMIT` time, strictly post-commit; there is a window where the row is visible to other readers but the notification has not yet reached subscribers). Other backends may have stronger or weaker guarantees.

**Consumer-side contract asymmetry.**
* Consumers relying on "the completion event arrived → therefore the execution is committed" MAY assume the implication (notify-after-commit is the guarantee). Downstream state observation after notification is safe.
* Consumers relying on "the execution is committed → therefore the notification arrived" MUST NOT assume the implication. The notification may not yet have reached them. Any such consumer must tolerate a visibility window and either poll on `describe_execution` for confirmation or accept eventual-consistency.

**Backend-side contract.** A backend's `complete` (and siblings) implementation MUST ensure the notification is scheduled within or after the commit — never before (never-pre-commit is the invariant that makes the consumer-side "notification implies commit" assumption safe). Valkey satisfies this trivially: the FCALL's PUBLISH is in the same Lua execution as the state mutation. Postgres satisfies this via `NOTIFY` inside the transaction (NOTIFY deliveries are deferred to commit; rollback cancels queued notifies).

**Outbox pattern: REQUIRED for non-transactional backends (round-4, M7).** The "never-pre-commit" invariant is mandatory. A backend whose underlying storage layer can deliver a notification atomically with the state transition (Valkey FCALL PUBLISH; Postgres `NOTIFY` inside a transaction) satisfies the invariant by construction; such backends do NOT need an outbox.

A backend whose storage layer CANNOT atomically deliver notification with commit — hypothetical NATS-backed, Kafka-backed, or split-storage designs — MUST implement a transactional outbox (persistent row written inside the commit; separate publisher process drains the outbox after commit is durable). Fire-and-forget publish BEFORE commit is a contract violation; fire-and-forget publish AFTER commit without an outbox is a correctness gap (on crash between commit and publish, the event is lost — violating the consumer-side "notify implies commit" contract's dual "commit eventually implies notify" that an outbox guarantees).

Round-2 said outbox was "optional." That wording was inconsistent with the mandatory never-pre-commit invariant: a non-transactional backend without an outbox either violates never-pre-commit (publish before commit) or drops notifications on crash (publish after commit without retry). Either is a contract violation. Round-4 resolves: outbox is a mechanism, and non-transactional backends MUST implement SOME mechanism (outbox is the standard one) to satisfy the invariant. Transactional backends are exempt because their native primitives satisfy the invariant without an outbox.

The contract the trait enforces is behavioural (the invariant), not structural (the mechanism). A backend that uses a different mechanism to satisfy the invariant is fine; a backend that uses no mechanism is out of contract.

**Why this matters 6 months out.** A cairn or external consumer writing logic of the form "I saw complete() return; therefore my peer-observer has been woken" will be broken on Postgres if we don't make the asymmetry explicit. The fix is not a weaker `complete` (it's atomic at commit) but an explicit named contract the consumer can code against.

---

## §4 Concrete trait types (round-2 — no associated types)

Round-1 used five associated types; round-2 replaces all five with concrete types at the trait boundary (K#5 resolution — §7.7 closed). The change makes the trait object-safe without requiring per-use-site type projection and eliminates the generics-proliferation cost across SDK wrapper signatures.

### §4.1 `Handle` — concrete opaque struct

```text
pub struct Handle {
    backend: BackendTag,
    kind: HandleKind,                 // Fresh | Resumed | Suspended — #[non_exhaustive]
    opaque: HandleOpaque,             // wraps Box<[u8]>; round-4 per §7.17, replacing bytes::Bytes
}

#[non_exhaustive]
pub enum HandleKind {
    Fresh,
    Resumed,
    Suspended,
}

pub struct HandleOpaque(Box<[u8]>);    // newtype; no `bytes` transitive dep
```

**`HandleKind` visibility + stability (round-4, M5).** `HandleKind` is `pub` and `#[non_exhaustive]`. Consumers that pattern-match MUST include a `_` arm; variant additions (e.g., `Detached`, `Orphaned` in a future RFC) are non-breaking. The variants are a useful debug/ops surface — a consumer inspecting a `Handle` for diagnostics knows whether it is fresh / resumed / suspended — which is the reason to leave `kind` pub-visible rather than sinking it into `opaque`. The `opaque` bytes remain backend-private; they are not pattern-matchable.

Owned by the worker for the duration of the attempt. Borrowed by every op (round-4 per M-D2; round-2 had terminal ops consume the handle, which broke the retry-after-transport-error story). Terminal ops (`complete`, `fail`, `cancel`) leave the handle valid per idempotent replay — a second call returns `EngineError::State { ... }` indicating "already terminal." `suspend` borrows the handle and returns a fresh Handle (kind=Suspended) the caller substitutes in place of their old cookie.

**Valkey impl:** the `opaque` bytes encode exec id, attempt id, lease id, lease epoch, capability binding, partition. Decode is backend-internal.

**Postgres impl:** `opaque` encodes row primary key + lease token + fence-triple.

**Why concrete, not opaque-associated-type:** associated types cannot be named through `dyn EngineBackend` without a per-use-site projection. A concrete struct with an internal opaque-bytes payload gives the same API-surface hiding (consumer never constructs a Handle; backend is the only writer) while remaining dyn-safe. Backends pay a serialize-on-create + parse-on-op cost; on Valkey this is microsecond-range (a small struct-pack), dominated by the FCALL round-trip.

**`HandleKind` runtime checking.** Round-1 used compile-time type distinctness (`ResumeHandle` vs `Handle`) to prevent threading a resumed handle into a path expecting a fresh one. Round-2 makes this a runtime check: the backend validates `kind` on entry to each op and returns `EngineError::State { expected, actual }` on mismatch. This matches the existing fence-triple runtime-check discipline — which, unlike the handle-kind split, we never proposed to make compile-time-enforced, because the runtime check is already cheap and the value is clear. (If reviewers revisit the cost/benefit and prefer the compile-time enum with a `Handle<Fresh>` / `Handle<Resumed>` phantom-typed approach, that's an additive change later; round-2 picks the simpler shape first.)

### §4.2 `EngineError` — concrete, Phase-1 landed (round-4 honest shape)

Backends return the concrete `EngineError` defined in `crates/ff-sdk/src/engine_error.rs`. See that file for the authoritative variant list and `#[non_exhaustive]` posture; every top-level variant and every sub-kind is `#[non_exhaustive]` so FF can add Lua-side error codes in minors without breaking consumers.

**Current variants (verbatim from `engine_error.rs`, round-4 per M2):**

```text
#[non_exhaustive]
pub enum EngineError {
    NotFound { entity: &'static str },
    Validation { kind: ValidationKind, detail: String },
    Contention(ContentionKind),
    Conflict(ConflictKind),
    State(StateKind),
    Bug(BugKind),
    Transport { #[source] source: Box<ScriptError> },
}
```

Round-2 paraphrased this enum and dropped `NotFound`; round-4 cites it authoritatively. The sub-kinds (`ValidationKind`, `ContentionKind`, `ConflictKind`, `StateKind`, `BugKind`) each carry a rich taxonomy — consumers match on the sub-kind, not on parsed strings. See the file for the full sub-kind lists.

**Stage 0 broadening of `Transport` (round-4, M1).** The current `Transport { source: Box<ScriptError> }` variant is tightly coupled to `ff_script::error::ScriptError` — a `ValkeyBackend::fail()` can wrap a ferriskey error into a ScriptError, but a future `PostgresBackend::fail()` cannot construct a ScriptError to wrap a `sqlx::Error`. The trait claims backend-agnosticism; the current variant shape contradicts it.

Stage 0 broadens the variant to:

```text
Transport {
    backend: &'static str,         // "valkey", "postgres", etc. — diagnostic label
    #[source]
    source: Box<dyn std::error::Error + Send + Sync + 'static>,
}
```

This is an additive-but-not-strictly-compatible change (existing callers constructing `Transport { source: ... }` still compile if the rename is done via `#[deprecated]` alias or a named constructor `EngineError::transport_script(err)`; plain struct-literal construction does need a touch-up). The Stage 0 PR handles the migration across ff-* in one commit. Consumers downstream that match on `Transport { source }` see a widened `source` type; downcasting to `ScriptError` still works for Valkey-backed callers via `source.downcast_ref::<ScriptError>()`.

**Stage 0 new variant: `Unavailable` (K#7 holdover).**

```text
Unavailable { op: &'static str }
```

Returned by staged backend impls for methods not yet wired up. Graceful degradation instead of `unimplemented!()` panics. Additive.

**`NotFound` is already in the enum.** Round-2's paraphrase dropped it; round-4 restores the authoritative citation. Consumers must match `NotFound { entity }` for 404-style lookups (e.g., `describe_execution` for a nonexistent id).

**Classification invariant.** Every backend must map its native errors into these structural variants deterministically. Valkey: `ScriptError` + `ferriskey::ErrorKind` classify per the existing `classify()` function (Phase 1). Postgres: `sqlx::Error` / `tokio_postgres::Error` classify per a new per-crate classifier (part of the Postgres-backend RFC). The invariant is a prose contract — Rust has no language-level constraint to enforce it. §7.9 debates enforcement via a `BackendError::classify()` sub-trait method; since `EngineError` is the concrete trait-level error, classification happens inside the backend before returning. §7.9 is therefore partially obsolete post-round-2.

### §4.3 `CompletionStream` — `Pin<Box<dyn Stream + Send>>`

Any trait method returning a completion-subscription stream (e.g., a future `CompletionBackend::observe_completions` method per #90) returns:

```text
Pin<Box<dyn Stream<Item = CompletionPayload> + Send>>
```

**Valkey impl:** wraps a ferriskey pubsub subscription on `ff:dag:completions`, filtered client-side.

**Postgres impl:** wraps a `tokio-postgres` LISTEN handle or a logical-replication tail.

**Why boxed, not associated:** associated `type CompletionStream: Stream<...>` made the trait non-dyn-safe (GAT-ish requirement). Boxing at the trait boundary costs one heap alloc per subscription (not per event); per-event perf is identical. The ergonomic/cost tradeoff favours the boxed form for the foreseeable throughput regime (subscriptions are per-consumer, events are per-completion).

**`CompletionPayload` type:** `ff_core::contracts::CompletionPayload` — not defined as of writing; ships with #90's scope. This RFC authorises its existence; the wire shape is #90's deliverable.

### §4.4 (reserved — round-1 SuspendToken/ResumeHandle collapsed into §4.1)

Round-1 had two additional associated types (`SuspendToken`, `ResumeHandle`). Round-2 collapses both into `Handle` with `HandleKind`. See §4.1.

---

## §5 Migration plan (round-4 — Stage 0 introduced, former Stages 1+2 fused)

The trait extraction is staged. Each stage is independently landable, with an acceptance gate and rollback posture. No single PR attempts the full migration.

Round-4 restructures the staging per M6 (fuse trait + config) and M1/M2/M3/M4 (acknowledge Stage 0 type-plumbing as its own stage):

* **Stage 0** — additive type plumbing + `EngineError` broadening + `ResumeSignal` crate move. Public-surface-touching but strictly additive / shim-compatible.
* **Stage 1** — trait extraction + `BackendConfig` in one landing (formerly Stages 1 & 2). Single migration event for consumers. Landing in sub-stages 1a → 1b (issue #119, shipped) → 1c.
* **Stage 2** (former Stage 3) — experimental `ff-backend-postgres`.
* **Stage 3** (former Stage 4) — cairn migration (cairn-team timeline).
* **Stage 4** (former Stage 5) — seal `ff_core::keys`.

**Round-7 amendment landing (parallel, independent of Stage 1c).** The #117 deferrals amendment (§R7) is NOT a Stage 1 sub-stage. It is a parallel, RFC-amendment-only landing: one commit touches trait signatures (`append_frame`, `report_usage`, new `create_waitpoint`), `ff-core::backend` type moves/introductions (`AppendFrameOutcome` move, `PendingWaitpoint` new, `AdmissionDecision` removed), `contracts.rs` (`ReportUsageResult` gets `#[non_exhaustive]`), and the Valkey backend impls (routes three methods to real bodies; `suspend` stays stubbed `EngineError::Unavailable`). Sequencing relative to Stage 1c is not constrained — either may land first; the round-7 deltas are shape-local. **Target release: 0.4.0** (breaking change on `report_usage` return type forces the minor bump on its own, independent of `suspend` deferral and any other Stage 1c scope).

### §5.0 Stage 0 — Type plumbing + `EngineError` broadening + `ResumeSignal` move

Stage 0 is prerequisite to Stage 1. Without it, Stage 1's trait signatures do not compile. It exists as its own named stage so the public-surface additions are reviewed as a cohort rather than buried in the trait-extraction PRs.

**Scope.**
1. Land the 13 new public types per §3.3.0 in `ff-core::contracts` and `ff-core::types` (as mapped). Each is additive; each lands with rustdoc + minimal unit tests.
2. Broaden `EngineError::Transport` per §4.2 to carry `Box<dyn std::error::Error + Send + Sync + 'static>` + a `backend: &'static str` tag. Provide a helper `EngineError::transport_script(err: ScriptError) -> Self` for the Valkey call sites to retain ergonomics. Migrate all `ff-*` construction sites in the same PR.
3. Add `EngineError::Unavailable { op: &'static str }` variant (round-2 K#7 holdover).
4. Move `ResumeSignal` from `ff-sdk/src/task.rs` to `ff-core::contracts`. Keep `pub use ff_core::contracts::ResumeSignal` in `ff-sdk::task` so `ff_sdk::task::ResumeSignal` continues to resolve through the 0.4.x window; schedule re-export removal for 0.5.0.
5. Optionally move `FailOutcome` on the same pattern (ff-sdk → ff-core::contracts with shim). Decision at impl time based on whether the struct is truly ff-core-shaped.

**What does not change:** trait, backends, SDK method names, wire shapes.

**Landing gate.** `cargo test --workspace` green. `cargo semver-checks` (if configured) flags no non-additive changes. Downstream cairn-fabric smoke build (against a pre-release version) green.

**Rollback.** All changes are additive; revert is a revert.

**Est.** 6-10h. Lower bound of §10's range because the scope is mechanical.

### §5.1 Stage 1 — Trait extraction + `BackendConfig` (fused, M6)

**Scope.**
1. Define `EngineBackend` trait in `ff-backend-api` (new crate per §7.8).
2. Move `WorkerConfig`'s `host/port/tls/cluster` fields behind a `BackendConfig` abstraction. Worker-policy fields stay on `WorkerConfig`. `WorkerConfig::connect_with(backend)` is the new construction path; `WorkerConfig::new(host, port, ...)` remains as a Valkey convenience shim that internally builds a `ValkeyBackendConfig` + constructs the backend.
3. The current `FlowFabricWorker` + `ClaimedTask` inherent impls become a `ValkeyBackend` implementation of the trait (new crate `ff-backend-valkey`, or module in `ff-sdk` with a feature flag — decision at impl time, not RFC-material).
4. SDK wrapper methods (`ClaimedTask::complete` etc.) become thin forwarders to the trait.

**Why fuse (M6).** Round-2 split trait-extraction from config-decoupling into two stages. Worker M correctly noted this forces two consumer migration events: once when they adopt the trait API, once when they want to swap backends via config. Pre-1.0 + single-consumer-mid-migration means two events is gratuitous cost. Fusing the two lands one coherent "the SDK is now backend-parametric" release.

**What does not change (post-fuse):** SDK method names. Existing wire. Existing `WorkerConfig::new(host, port, ...)` call sites continue to compile. Consumers who don't touch the trait / don't want to swap backends are unaffected.

**What does change:** the trait exists as a new public surface; `BackendConfig` replaces `WorkerConfig`'s connection fields; consumers who want to pass a pre-constructed backend have an explicit API for it.

**Landing gate.** `cargo test --workspace` green. `ff-test` green against real Valkey. Example apps (`examples/media-pipeline`, `examples/coding-agent`) green without edits. Cairn-fabric smoke build green.

**Rollback.** Revert-to-main is clean because Stage 0 (which bundles the public-surface-additive work) remains landed; the revert touches only trait definitions + SDK wiring.

**Est.** 25-35h (slightly higher than round-2's Stage 1 at 20-30h because the fused scope includes the BackendConfig move).

### §5.2 Stage 2 — Experimental `ff-backend-postgres` (formerly Stage 3)

**Prerequisite.** Stage 0 + Stage 1 landed.

**Scope.** A new crate `ff-backend-postgres` lands with a minimal `EngineBackend` impl. Minimum-surface: `claim` / `renew` / `progress` / `append_frame` / `complete` / `fail` / `describe_execution`. Unimplemented methods return `Err(EngineError::Unavailable { op: "<method_name>" })` — NOT `unimplemented!()`.

*Round-2 note (K#7):* round-1 said unimplemented methods "remain `unimplemented!()`." `unimplemented!()` panics, which in an async runtime propagates to the task and, for a library crate, gives a consumer evaluating the crate a process-level panic with no recovery path. Replacing with `Err(EngineError::Unavailable { op })` gives graceful degradation: consumers receive a typed error they can match on and fall back (or report clearly to the user). The booby-trap is removed.

**This is not a shippable Postgres backend.** It is a proof-of-concept that the trait shape is implementable against a non-Valkey backend. Its existence validates that the Stage 1 trait design did not accidentally bake in Valkey-specific assumptions; if it did, Stage 1 reopens.

**Landing gate.** Integration test suite against a Postgres 15 container for the implemented method set. `Unavailable` returns are exercised once each to confirm they do not panic.

**Non-ship contract.** The crate ships under `[features]` or with a `README` that says "experimental; non-production." No version promise.

### §5.3 Stage 3 — Cairn-fabric migration (cairn-team timeline, formerly Stage 4)

Cairn migrates at their own pace. Our side: keep the `EngineBackend` trait stable (no breaking changes post-Stage 1 without a minor version bump), provide the `ff-backend-valkey` crate, provide the migration guide. Cairn's team decides when to adopt.

No gate on our side. This stage completes when cairn files a PR closing their migration.

### §5.4 Stage 4 — Seal `ff_core::keys` as `pub(crate)` (formerly Stage 5)

**Scope.** Once no internal `ff-*` crate imports from `ff_core::keys` (the raw Valkey key-name builders — `ExecKeyContext`, `IndexKeys`, etc.) outside the backend crates, flip the module visibility to `pub(crate)` inside the `ff-backend-valkey` crate. External consumers are fully abstracted from Valkey key names.

**Landing gate (INTERNAL).** Before flipping visibility, run an internal verification step:

1. Grep the ff workspace for `ff_core::keys::*` public uses outside `ff-backend-valkey` (and `ff-core` itself). Every match must be migrated to a trait-level method or vendored into the caller's own tooling module.
2. Run `cargo check -p <each ff-* crate>` against the sealed module. Any compile failure indicates an uncaught internal dependency that must be addressed before seal.
3. Run the workspace integration test suite (`cargo test --workspace`) with the sealed module. Green on all tests.

This is an internal-only gate. Cairn's `ff_core::keys` usage (if any, per the cairn-blocking work tracker) is on cairn's migration timeline and does NOT gate the seal per the peer-team-boundaries discipline (owner decision: cairn-no-gate for cross-team coordination).

*Round-2 note (K#8 reframing):* round-1 made the seal contingent on cairn's clean compile, which contradicts the owner's locked cairn-no-gate decision. Round-2 reframes: the seal is gated on *our* internal cleanliness. If cairn still has non-hot-path `ff_core::keys` usage post-seal (test CLI, diagnostic tooling), we produce a migration guide as a peer-team artifact; we do not modify cairn code. If cairn can't migrate in time for their own release cadence, they can pin to a pre-seal version of `ff-core` until ready. Our seal discipline is ours; their adoption discipline is theirs.

**Rollback.** Revert the visibility flip if an internal use case surfaces that the trait doesn't cover — the trait needs another method, not a leak of keys. Exception path: a worker that builds its own diagnostic tooling on raw keys. If that emerges, we expose a `DiagnosticsBackend` trait or a typed key-view API; we do not re-open `pub keys`.

---

## §6 Alternatives rejected

### §6.1 FCALL-mirror granularity (18 methods, 1:1 with Lua)

**Rejected per owner decision (§1.5.1).** The trait would have 18 methods matching the 18 current Lua functions one-to-one: `ff_issue_claim_grant`, `ff_claim_execution`, `ff_claim_resumed_execution`, `ff_complete_execution`, `ff_fail_execution`, `ff_cancel_execution`, `ff_renew_lease`, `ff_update_progress`, `ff_append_frame`, `ff_report_usage_and_check`, `ff_create_pending_waitpoint`, `ff_suspend_execution`, `ff_deliver_signal`, `ff_delay_execution`, `ff_move_to_waiting_children`, `ff_cancel_flow`, `ff_add_execution_to_flow`, plus the per-scanner admission/reconciler variants.

**Why rejected:**

* It locks the Postgres (and any future) backend into pretending to be Lua. A Postgres impl of `ff_report_usage_and_check` has to do the FCALL's exact sequence of budget read → admission decision → usage update → result return, because that's the contract the 18-method trait encodes. But a Postgres backend could run that in a single `UPDATE ... RETURNING` — if the contract allows it.
* The trait surface is a user-facing concept even though the impl side is internal. Callers reading the trait see 18 methods with FCALL-shaped names. The ergonomic cost of the narrower granularity compounds: every caller pattern-matches against the FCALL taxonomy instead of the business taxonomy.
* Changes to Lua (adding an FCALL, merging two) force trait-method churn. The trait ossifies the Lua implementation instead of abstracting it.

A 10-12 method trait expresses business operations. The Valkey backend composes FCALLs internally to fulfil each. This is the right abstraction level per §3.1's decision.

### §6.2 Multiple smaller traits (one per domain)

**Rejected.** Splitting into `ClaimBackend`, `LifecycleBackend`, `SuspendBackend`, `DescribeBackend`, `BudgetBackend` forces every SDK consumer to compose multiple trait-object references or multiple generic parameters. The SDK ergonomics suffer for a structural organisation that the user doesn't care about.

**Exception:** admin-surface ops live on a separate `AdminBackend` trait (§3.2), because admin callers (operator tooling, cutover scripts) are categorically different consumers than worker code. Two traits is acceptable when the consumer class is different.

**Exception:** `StreamBackend` (issue #92 scope) lives as a separate trait because the stream-cursor abstraction is structurally different (cursor-typed). Two traits because the type system says they are different.

**Exception:** `CompletionBackend` (issue #90 scope) lives separately for the same reason as StreamBackend — the subscription abstraction is different from the op-dispatch abstraction.

Where the rationale gives a clear single-consumer-class + shared-type-category signal, one trait. Where the rationale splits along a consumer-class boundary or a type-category boundary, two traits.

### §6.3 Leave coupling as-is, ship Postgres as a parallel server binary

**Rejected.** A parallel `ff-server-postgres` binary that implements the same REST API but with a Postgres backend internally would avoid the trait extraction entirely. Consumers pick their server at deploy time; the SDK is thin.

**Why worse:**

* Two server binaries with the same semantics but different impls = two codebases that drift. Every Lua-side correctness improvement (fence-triple discipline, atomicity fixes, etc.) has to be ported to the Postgres-side SQL impl manually. Drift is inevitable.
* SDK consumers who embed the engine in-process (RFC-010 architecture allows this; cairn embeds) lose the option — they cannot swap backends at compile time without the trait.
* The REST-only surface discards the whole point of the SDK being a Rust-native consumer experience. Embedding-mode users are forced into RPC.

### §6.4 Trait-objects only, no generic parameters

**Round-2 status: resolved.** Round-1 left this as "usable both ways" (generic + dyn). The round-2 concrete-types decision (§4, K#5) makes both usable simultaneously without trade-off: there are no associated types to project, `dyn EngineBackend` works directly, and `<B: EngineBackend>` works directly. The dispatch choice is 100% consumer-side. §7.7 is closed accordingly.

### §6.5 Single `terminate(handle, Outcome)` method in place of `complete` / `fail` / `cancel` (round-4, M-D3)

**Rejected.** A fused terminal op `terminate(handle, outcome: TerminalOutcome)` where `TerminalOutcome` is an enum like `{ Complete { payload }, Fail { reason, class }, Cancel { reason } }` would give one method in the trait instead of three. Considered and rejected:

* Each of the three has a genuinely different signature + return type (`complete` returns `Result<(), _>`, `fail` returns `Result<FailOutcome, _>` with retry-scheduling info, `cancel` returns `Result<(), _>`). Collapsing forces the return type to an enum too (`TerminalResult { NoRetry, Retry { delay } }` and the always-`None` arms for `complete`/`cancel`) — more clutter, not less.
* Variant-shaped per-arm payload pattern is clumsier at call sites than three named methods. `backend.complete(h, payload)` reads better than `backend.terminate(h, TerminalOutcome::Complete { payload })`.
* The three ops map to three different Valkey FCALLs (`ff_complete_execution`, `ff_fail_execution`, `ff_cancel_execution`) and three different Postgres transactions — there is no backend-internal simplification to gain.

The three-method shape stays.

### §6.6 Shared implementations across backends via a base trait

**Rejected as premature.** A hierarchy like `trait Backend { ... default impls ... }` + `trait EngineBackend: Backend` sharing commonalities across future backend impls (e.g., fence-triple verification logic) is worth considering once a second backend exists. Today, exactly one backend exists (Valkey); the default-impl slots would all be `unimplemented!()` placeholders. Deferred to post-Stage-3 when the Postgres backend has materialised and shared surfaces become visible.

---

## §R7 Round-7 amendment — Stage 1b deferral resolutions (issue #117)

This appendix is the permanent record of the round-7 amendment. Top-of-RFC summary lives in the "Round-7 amendment summary" block above; the full reasoning, evidence log, and alternatives-considered trail live here. Challenger discipline (K → L → M) is preserved in `rfcs/drafts/RFC-012-amendment-117-deferrals.{K,L,M}-challenge.md`.

**Tracks:** issue [#117](https://github.com/avifenesh/FlowFabric/issues/117). Originates from RFC-012 Stage 1b (shipped `6f54f9b`, PR #119 — 8 of 12 `ClaimedTask` ops migrated).
**Shape:** trait-surface breaking. Widens 1 existing method return type, replaces 1 existing method return type, adds 1 new method, adds 1 new type in `ff-core::backend`, adds `#[non_exhaustive]` to 1 existing contract enum. No Lua / wire changes. Pre-1.0 posture: CHANGELOG-only communication, no BC shims beyond `pub use` path-preservation.
**Base commit:** `da89fa9` (post-0.3.2 hotfix). Ships 3 of 4 deferred methods; `suspend` deferred wholesale to Stage 1d.
**Manager adjudication on editorial opens (this PR):** §R7.6.2 — insert `create_waitpoint` as method 16 in §3.1.1 alongside other lifecycle ops (no new sub-bucket); §R7.6.3 — keep `create_waitpoint` name (rustdoc cites Lua `use_pending_waitpoint` ARGV flag for disambiguation); §R7.6.4 — envelope prose not rewritten; "15 in spirit" envelope relaxed to "17 current trait methods" as of Round-5+Round-7 (noted in this summary and the Round-7 top-of-RFC summary).

### §R7.1 Motivation

Stage 1b migrated 8 of the 12 `ClaimedTask` SDK methods onto the `EngineBackend` trait as thin forwarders (`crates/ff-sdk/src/task.rs:166-178` docstring). Four methods could not land as thin forwarders: three because the trait's return type is strictly less expressive than the SDK's, and one because the trait has no slot at all.

This amendment ships **three of the four**. `suspend` is deferred to Stage 1d:

| # | SDK site | SDK return | Trait return | Gap | Addressed here? |
|---|----------|-----------|--------------|-----|-----------------|
| 1 | `ClaimedTask::create_pending_waitpoint` (`task.rs:686`) | `(WaitpointId, WaitpointToken)` | **no slot** | Trait lacks a pending-waitpoint op entirely. | **Yes** — §R7.2.2 |
| 2 | `ClaimedTask::append_frame` (`task.rs:740`) | `AppendFrameOutcome { stream_id, frame_count }` (`task.rs:136-143`) | `()` (`engine_backend.rs:103`) | Trait discards backend-emitted stream id + frame count. | **Yes** — §R7.2.1 |
| 3 | `ClaimedTask::suspend` (`task.rs:812`) | `SuspendOutcome::{Suspended, AlreadySatisfied}` (`task.rs:72-90`) | fresh `Handle` (`engine_backend.rs:129-134`) | Trait cannot express the `AlreadySatisfied` (lease-retained) branch; return-type widening entangles with the SDK wire parser. | **Deferred — Stage 1d** |
| 4 | `ClaimedTask::report_usage` (`task.rs:629`) | `ReportUsageResult::{Ok, AlreadyApplied, SoftBreach{…}, HardBreach{…}}` (`contracts.rs:1400-1418`) | `AdmissionDecision::{Admitted, Throttled{…}, Rejected{…}}` (`backend.rs:367-376`) | Variant sets don't overlap; structured breach fields collapse to `Rejected{reason: String}`. | **Yes** — §R7.2.3 |

**Why `suspend` defers.** Round-3 review (M1) established that any trait `SuspendOutcome` shape that preserves the existing Round-4 `HandleKind::Suspended` contract (`engine_backend.rs:126-128`, `backend.rs:50,788`) entangles with the SDK's internal wire parser `parse_suspend_result` at `crates/ff-sdk/src/task.rs:1465,1557-1562`, which constructs `Suspended` exhaustively from wire bytes with no access to `Handle`'s opaque payload (`backend.rs:60-69`). The only clean options are (a) rewrite the SDK forwarder in the same PR — which in turn requires the `ConditionMatcher`↔`WaitpointSpec` input-shape work (§R7.6.1) — or (b) ship a weakened `Option<Handle>` shape. Both defeat the amendment's thin-forwarder intent. Stage 1d does the input shape anyway; `suspend` rides with it.

Each TODO at the SDK site references `#117`. The `report_usage`, `append_frame`, and `suspend` trait impls are stubbed `EngineError::Unavailable` at `crates/ff-backend-valkey/src/lib.rs:1145-1147, 1173-1180, 1240-1249` — evidence the current trait shape is not usable, not merely unused.

**Why round-7 and not Stage 1d-as-planned.** RFC-012 Stage-1 landing gate said "no public-surface change beyond additive" (§5.1, §3.3.0). Fixing these three requires breaking two existing trait signatures and adding one method. That's a round-7 amendment, not an implementation detail.

### §R7.2 Proposed trait deltas

#### §R7.2.1 `append_frame` — widen return

**Current** (`crates/ff-core/src/engine_backend.rs:103`):
```rust
async fn append_frame(&self, handle: &Handle, frame: Frame) -> Result<(), EngineError>;
```

**Proposed:**
```rust
async fn append_frame(
    &self,
    handle: &Handle,
    frame: Frame,
) -> Result<AppendFrameOutcome, EngineError>;
```

**Type move.** `AppendFrameOutcome` moves from `ff-sdk::task` (`task.rs:137-143`) to `ff-core::backend`, mirroring the Stage-1a `FailOutcome` move (`backend.rs:546-558`, `task.rs:152`: `pub use ff_core::backend::FailOutcome`). Keep a `pub use` shim at `ff_sdk::task::AppendFrameOutcome` through 0.4.x. Derive set widens from SDK's `Clone, Debug` to match `FailOutcome` precedent (§R7.5.6):

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppendFrameOutcome {
    pub stream_id: String,
    pub frame_count: u64,
}
```

**No `#[non_exhaustive]`.** Today's `AppendFrameOutcome` construction is internal to `ff-sdk` (parser at `task.rs:1747`); future external constructors are not anticipated, matching `FailOutcome`'s consumer-shape rationale (`backend.rs:546-550`). Consumer-shape evidence, not just FailOutcome-consistency (MN3). Diverges from v1's silent addition (K3).

**Rationale.** `append_frame`'s backend emission (Valkey XADD returns entry id, XLEN returns frame count — SDK parser at `task.rs:1678`) carries real caller-useful state.

**Placement in §3.1.1.** Method 3b, unchanged.

**Breakage scope.** Current `ff-backend-valkey::append_frame` returns `Err(EngineError::Unavailable { op: "append_frame" })` (`crates/ff-backend-valkey/src/lib.rs:1145-1147`) — a stub, not `()`-success. Zero direct trait-level consumers.

#### §R7.2.2 `create_waitpoint` — new trait method

**Current.** No slot. `ClaimedTask::create_pending_waitpoint` (`crates/ff-sdk/src/task.rs:686-728`) bypasses the trait via `self.client.fcall("ff_create_pending_waitpoint", …)`.

**Proposed new method:**
```rust
/// Issue a pending waitpoint for future signal delivery.
///
/// Waitpoints have two states in the Valkey wire contract:
/// **pending** (token issued, not yet backing a suspension) and
/// **active** (bound to a suspension). This method creates a waitpoint
/// in the **pending** state. A later `suspend` call transitions a
/// pending waitpoint to active (see Lua `use_pending_waitpoint` ARGV
/// flag at `flowfabric.lua:3603,3641,3690`) — or, if buffered signals
/// already satisfy its condition, the suspend call returns
/// `SuspendOutcome::AlreadySatisfied` and the waitpoint activates
/// without ever releasing the lease.
///
/// Pending-waitpoint expiry is a first-class terminal error on the wire
/// (`PendingWaitpointExpired` at `ff-script/src/error.rs:170,403-408`).
/// The attempt retains its lease while the waitpoint is pending;
/// signals delivered to this waitpoint are buffered server-side.
async fn create_waitpoint(
    &self,
    handle: &Handle,
    waitpoint_key: &str,
    expires_in: Duration,
) -> Result<PendingWaitpoint, EngineError>;
```

**New type in `ff-core::backend`:**
```rust
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PendingWaitpoint {
    pub waitpoint_id: WaitpointId,
    pub hmac_token: WaitpointHmac,
}
```

`WaitpointHmac` already exists at `crates/ff-core/src/backend.rs:247-277` — Debug-redacts, **no Display impl** (KN2 fix verified).

**Naming decision — L2 resolved as option (c), manager adjudicated as final (§R7.6.3).** Keep `create_waitpoint`. v2's "pending is Valkey-internal" was factually wrong — "pending" is load-bearing in Rust contract code (`ff-script/src/error.rs:170,403-408`) and Lua (`flowfabric.lua:801-823,3641,3690`). Correct rationale:
- The method creates the pending-kind exclusively; the docstring above disambiguates vs `suspend`'s waitpoint-activation path.
- The trait surface describes the caller's business op ("issue a waitpoint I'll hand to a signal deliverer"), not the internal state machine. Symmetry with `suspend` as the activation path lives in prose, not in the method name.
- Alternatives considered: (a) `create_pending_waitpoint` — verbose, matches SDK; (b) `issue_pending_waitpoint` — echoes "issue" but trait peer is `claim_from_reclaim`. Either is acceptable. `create_waitpoint` + explicit docstring wins on brevity.

**Placement in §3.1.1** (lifecycle ops, KD4 accepted — no new sub-bucket; manager adjudicated §R7.6.2). RFC-§3.1 taxonomy count: 15 (pre) → 16 (post); `async fn` compile count: 16 (pre) → 17 (post). Both framings stated per K4 / LN2.

**Rationale for placement vs alternatives.**
- *Alt A: fold into `suspend`.* Rejected: SDK flow creates the waitpoint first (hand token to external signal deliverer), `suspend` later. Folding breaks the external-delivery handshake.
- *Alt B: `AdminBackend`.* Rejected: worker-scoped op (requires lease-holder handle).
- *Alt C: leave off-trait permanently.* Rejected: violates §5.4 sealability precondition.

#### §R7.2.3 `report_usage` — replace `AdmissionDecision` with `ReportUsageResult`

**Current** (`crates/ff-core/src/engine_backend.rs:192-197`):
```rust
async fn report_usage(&self, handle: &Handle, budget: &BudgetId,
    dimensions: UsageDimensions) -> Result<AdmissionDecision, EngineError>;
```
with `AdmissionDecision::{Admitted, Throttled{retry_after_ms}, Rejected{reason}}` at `backend.rs:367-376`.

**Proposed:**
```rust
async fn report_usage(&self, handle: &Handle, budget: &BudgetId,
    dimensions: UsageDimensions) -> Result<ReportUsageResult, EngineError>;
```

`ReportUsageResult` defined at `crates/ff-core/src/contracts.rs:1400-1418` (`Ok | SoftBreach{…} | HardBreach{…} | AlreadyApplied`).

**K1 fix: add `#[non_exhaustive]` to `ReportUsageResult` in this same PR.** Verified missing today (first `non_exhaustive` in `contracts.rs` is at line 1803 on an unrelated struct). Without it, replace-over-widen collapses. Landed atomically with the return-type swap in a single commit per M3 (§R7.7).

**`AdmissionDecision` fate.** Delete. In-tree references: type def (`backend.rs:367-376`), trait sig (`engine_backend.rs:192-197`), stub impl (`ff-backend-valkey/src/lib.rs:1240-1249`), three test constructions (`backend.rs:888-890`). All move together.

**Rationale — replace vs widen.** `AdmissionDecision`'s `Throttled`/`Rejected` variants are emitted by nobody (Lua `ff_report_usage_and_check` emits `Ok / ALREADY_APPLIED / SOFT_BREACH / HARD_BREACH` per `lua/budget.lua:99` + parser `task.rs:1194-1213`). `#[non_exhaustive]` preserves future additivity. `CheckAdmissionResult` (`contracts.rs:1447-1457`) already owns admission-rails (KD3).

**Placement in §3.1.3.** Method 13, signature change only.

### §R7.3 Migration path

**External consumers** of the three methods: none. No direct `EngineBackend` impls outside `ff-backend-valkey` (in-tree verifiable; cross-repo claim dropped per KD2).

**Internal consumers, one PR (atomic single commit per M3):**

1. `crates/ff-backend-valkey/src/lib.rs` — route `append_frame` and `report_usage` impls to real bodies (currently stubbed `Unavailable`); add `create_waitpoint` impl. `suspend` impl untouched (still `Unavailable` — migration deferred to Stage 1d).
2. `crates/ff-sdk/src/task.rs` — collapse `append_frame`, `create_pending_waitpoint`, `report_usage` TODO-#117 sites into trait forwarders. `suspend` site stays direct-FCALL (Stage 1d).
3. `crates/ff-core/src/backend.rs` — delete `AdmissionDecision`, move `AppendFrameOutcome` in from `ff-sdk::task`, add `PendingWaitpoint`.
4. `crates/ff-core/src/contracts.rs` — add `#[non_exhaustive]` to `ReportUsageResult` at line 1400.
5. `crates/ff-core/src/engine_backend.rs` — touch 3 trait methods (2 updates, 1 add); re-verify `_assert_dyn_compatible` at `:205-206`.
6. `crates/ff-sdk/src/task.rs` top — add `pub use ff_core::backend::AppendFrameOutcome` shim, matching `FailOutcome` at `:152`.

**Envelope prose (§R7.6.4, manager adjudicated).** Do not rewrite earlier round prose in §3.1. The top-of-RFC Round-7 summary and §3.1's updated intro both note the envelope is relaxed to "17 current trait methods" as of Round-5+Round-7. No separate §3.1-prose edit needed.

**CHANGELOG entry (0.4.0) — drafted for the trait-implementation PR, NOT this doc-only amendment PR:**
> **Breaking — `EngineBackend`:** `append_frame` now returns `AppendFrameOutcome`; `report_usage` now returns `ReportUsageResult` (replaces `AdmissionDecision`). New trait method `create_waitpoint` (trait grows 16→17 `async fn`; RFC-taxonomy 15→16). `ReportUsageResult` gains `#[non_exhaustive]`. `AdmissionDecision` removed. `suspend` migration deferred to a later release (tracked under #117 Stage 1d). External consumers: none. See #117.

### §R7.4 Non-goals

- Not migrating `suspend` (deferred to Stage 1d per §R7.1 / §R7.6.1).
- Not changing `UsageDimensions.dedup_key` reservation.
- Not reserving variants on `ReportUsageResult` for hypothetical rate-limit backends. `#[non_exhaustive]` handles future additions (K1).
- Not adding `AdminBackend::create_waitpoint`. Pending-waitpoint creation is worker-scoped.
- Not extending `report_usage` to carry admission rails. `CheckAdmissionResult` already owns that role (KD3).
- Not touching Lua. Wire shapes unchanged.
- Not editing CHANGELOG in this PR (doc-only; actual 0.4.0 CHANGELOG entries land with the trait-implementation PR).

### §R7.5 Alternatives considered

#### §R7.5.1 `report_usage` replace vs widen
Replace chosen (§R7.2.3). Widen rejected under K1-fix. KD3 citation: `CheckAdmissionResult` at `contracts.rs:1447-1457`.

#### §R7.5.2 `create_waitpoint` new method vs fold
New method (§R7.2.2 Alt A/B/C).

#### §R7.5.3 `append_frame` widen vs `()` + separate stream_position op
Widen. Two-op variant contradicts §3.4 atomicity.

#### §R7.5.4 Split into two amendments
Considered. Rejected: single landing PR, single CHANGELOG. (With `suspend` already deferred, the amendment is already the smaller of the two candidate splits.)

#### §R7.5.5 Defer the whole amendment to Stage 1d
Rejected. `report_usage`'s `AdmissionDecision → ReportUsageResult` swap is independent of `suspend`'s input-shape entanglement; ship what can ship. Stage 1d handles `suspend` only.

#### §R7.5.6 Canonical derive posture for trait outcome types (LD3)
`FailOutcome` (`backend.rs:546-558`), `AppendFrameOutcome`, `PendingWaitpoint` all use `Clone, Debug, PartialEq, Eq`. `ReportUsageResult` adds `Serialize, Deserialize` (wire parser corroboration). `#[non_exhaustive]` for types new-to-trait (`PendingWaitpoint`) or already-public-and-match-consumed (`ReportUsageResult`); absent for `FailOutcome` / `AppendFrameOutcome` (construction is SDK-internal today per MN3; exhaustive-construction desired).

**Shape-commitment note (MD2).** `AppendFrameOutcome.stream_id: String` is a stable shape assumption — the field carries Valkey Stream entry ids (e.g. `1234567890-0`). A future normalisation (typed `StreamId` newtype, suffix-stripping) would be its own breaking change. Flagged so a later author isn't surprised.

### §R7.6 Open questions

#### §R7.6.1 `suspend` deferred — Stage 1d tracking
`suspend`'s return-type widening (`SuspendOutcome`) and input-shape rework (SDK `&[ConditionMatcher]` + `TimeoutBehavior` → trait `Vec<WaitpointSpec>` + `Option<Duration>`, or a typed `ResumeCondition`) land together in Stage 1d. Entanglement with SDK `parse_suspend_result` at `task.rs:1465,1557-1562` makes partial migration net-negative (M1). **Owner question (Stage 1d):** typed `ResumeCondition` on trait in round-8, or ARGV-JSON in backend impl?

#### §R7.6.2 `create_waitpoint` §3.1 placement — closed
Manager adjudication (this PR): insert as method 16 in §3.1.1 alongside other lifecycle ops. Do NOT create a new §3.1.4 "waitpoint management" sub-bucket — one method doesn't warrant one. Trait async-fn count goes 16 → 17.

#### §R7.6.3 `create_waitpoint` naming — closed
Manager adjudication (this PR): keep `create_waitpoint`. Do not veto to `create_pending_waitpoint`. Rustdoc explicitly cites the Lua `use_pending_waitpoint` ARGV flag as context (§R7.2.2; per Worker Z-v3's fix).

#### §R7.6.4 Envelope prose — closed
Manager adjudication (this PR): acknowledge the "15 in spirit" envelope is relaxed to "17 current trait methods" as of Round-5+Round-7. Do not rewrite earlier round prose; the top-of-RFC Round-7 summary and §3.1's updated intro carry the note.

(Previously v3 §R7.6.3, §R7.6.4, §R7.6.5, §R7.6.7 closed. v3 §R7.6.6 closed in-tree. v3 §R7.6.8 renumbered to §R7.6.4.)

### §R7.7 Landing plan

**Stage.** Parallel RFC-amendment-only landing, independent of Stage 1c (§5 stage-map addendum).

**Atomicity.** Single commit, single 0.4.0 release cut. `ReportUsageResult`'s `#[non_exhaustive]` addition and the `report_usage` return-type replacement are one breaking-change event; no semver-two-step (M3). The release lane is 0.4.0 regardless of other open questions — `report_usage`'s `AdmissionDecision → ReportUsageResult` swap is unambiguously breaking and forces the minor bump on its own (MD1).

**Acceptance gate.**
1. All three in-amendment methods route through `EngineBackend` (no remaining `self.client.fcall("ff_*", …)` in `ClaimedTask` for `append_frame`, `create_pending_waitpoint`, `report_usage`). `suspend` not covered (deferred to Stage 1d).
2. `cargo test --workspace` green; `ff-test` green against real Valkey.
3. a. Static: `_assert_dyn_compatible` + `ValkeyBackend::_dyn_compatible` compile cleanly.
   b. Runtime: `Arc<dyn EngineBackend>` smoke test exercising `create_waitpoint` dispatches and returns a valid `PendingWaitpoint`.
4. CHANGELOG entry drafted for 0.4.0 (lands with the trait-implementation PR, not this doc-only amendment PR).

**Rollback.** Clean. Stage 0 + 1a/b types stay. Three-method delta + one attribute-addition unwind mechanically.

**Estimated effort.** 7-10h (3-delta scope; suspend's 3-6h deferred with it).

**Challenger discipline.** K applied (v1→v2); L applied (v2→v3); M applied (v3→v4). Three rounds sufficient — M1 was the last structural finding and was decidable in-place.

### §R7.8 Evidence log

All line numbers against `da89fa9`; suspend-related entries retained only where `suspend` deferral is justified.

- `crates/ff-sdk/src/task.rs:136-143` (`AppendFrameOutcome`), `:152` (`FailOutcome` shim), `:169` (Stage-1b doc), `:629-669` (`report_usage` + #117 TODO), `:686-728` (`create_pending_waitpoint`), `:740-789` (`append_frame`), `:1194-1213` (`parse_report_usage_result`), `:1465` (`parse_suspend_result` entry — referenced only to justify suspend deferral), `:1557-1562` (exhaustive `SuspendOutcome::Suspended` construction — the M1 evidence), `:1678` / `:1747` (`AppendFrameOutcome` parser + constructor).
- `crates/ff-core/src/engine_backend.rs:103` (`append_frame` sig), `:192-197` (`report_usage` sig), `:205-206` (`_assert_dyn_compatible`). 16 `async fn` by `grep -c "^    async fn " …`.
- `crates/ff-core/src/backend.rs:60-69` (`Handle` opaque payload — referenced in M1 deferral justification), `:247-277` (`WaitpointHmac` — Debug only, no Display), `:367-376` (`AdmissionDecision`, `#[non_exhaustive]`), `:546-558` (`FailOutcome` precedent), `:888-890` (`AdmissionDecision` test constructions).
- `crates/ff-core/src/contracts.rs:1400-1418` (`ReportUsageResult` — NOT `#[non_exhaustive]` today), `:1447-1457` (`CheckAdmissionResult`).
- `crates/ff-backend-valkey/src/lib.rs:1145-1147, 1173-1180, 1240-1249` (`append_frame` / `suspend` / `report_usage` stubs — `Unavailable`).
- `crates/ff-script/src/error.rs:170` (`PendingWaitpointExpired`), `:403-408` (`WaitpointNotTokenBound` TERMINAL doc).
- `crates/ff-script/src/flowfabric.lua:801-823` (`validate_pending_waitpoint` — pending vs active), `:3603` (ARGV contract), `:3641` (`use_pending_waitpoint` parse), `:3690` (pending activation branch).

---

## §7 Open questions

The questions below are listed for the debate record. Several have a clear right answer in context, but per the worker-I directive ("resist the urge to pre-decide") each is recorded for reviewer input before closure.

### §7.1 Split `claim` into `issue_grant` + `claim_execution`, or keep combined?

**The choice.** The combined `claim` method in §3.1.1 is one method; today's code has two separate FCALLs (`ff_issue_claim_grant` → `ff_claim_execution`). The two FCALLs correspond to two user-visible paths — the scheduler does the first, the worker does the second. Under the trait, collapsing to one method means the scheduler side is a trait-impl-internal concern, not a consumer-callable op.

**Arguments for combining.**
- Worker-facing callers only see "I got work" — the two-FCALL split is internal routing optimisation.
- Postgres backend has no natural split; forcing two trait methods bakes in Valkey's routing model.
- The scheduler side is a `ff-scheduler` crate concern, not an `EngineBackend` concern — it sits *above* the trait.

**Arguments for splitting.**
- Some consumers (future custom scheduler impls outside the ff-scheduler crate) want the pre-staging capability separately. Forcing them to call into `ff-scheduler` is an extra dep.
- Testability: a test that wants to simulate pre-staged grants without running a real scheduler can call `issue_grant` directly.

**Lean (not locked):** combined, with an escape hatch via `AdminBackend::issue_claim_grant` (not on `EngineBackend`) for the rare caller.

**Round-2 addendum (K#3 — reclaim third path).** Round-1 §7.1 only addressed the grant-issue-vs-claim split; K correctly noted today's code has a third path (`claim_from_reclaim_grant`) that round-1 silently erased. Round-2 resolution:

* `claim` (method 1) is fresh-work only. Grant-issue + execution-claim combined within the method, as round-1 proposed.
* `claim_from_reclaim` (method 9, new) is reclaim-resumed work only. Consumes a `ReclaimGrant` issued by the reclaim scanner after crash detection. The `ReclaimGrant` / `ClaimGrant` wire-type split is preserved (contracts.rs:108 vs :161 are semantically distinct, per K's read).
* `claim_via_server` is NOT on the trait. It is an HTTP orchestration concern that lives on the server/scheduler crate above the trait.

Three entry points, three distinct code paths, no fusion. The scheduler/reclaim separation is preserved as wire-visible types (`ClaimGrant` vs `ReclaimGrant`).

### §7.2 Capability routing — bitfield or stringly-typed?

**The choice.** `CapabilitySet` — the type threading through `claim`'s signature. Today capabilities are `Vec<String>` on `WorkerConfig` (per `crates/ff-sdk/src/config.rs`) and string-matched at the Lua level. Bitfields would be tighter; stringly-typed is more flexible.

**Lean:** stringly-typed (matches today). Debate: whether `CapabilitySet` should be a newtype over `Vec<String>` or an `HList`-style typed representation.

### §7.3 Does `suspend` return a fresh `Handle` on resume, or does `Handle` itself carry suspend state?

**Round-2 status: resolved — option (b).** Round-1 leaned (a) with (b) as fallback. Round-2 K#5 forced the `dyn`-safety resolution, which cascaded into collapsing `Handle`/`ResumeHandle`/`SuspendToken` into a single concrete `Handle` with internal `HandleKind` discriminator (§4.1). The compile-time safety we hoped to get from distinct types is replaced by a runtime `kind` check; this matches the existing runtime fence-triple discipline. §7.3 closed in favour of (b).

### §7.4 Do we need a `Batch` op for multi-execution submission?

**The choice.** The current SDK has no batch-submit — each `create_execution` is one call. A future high-throughput consumer might want `submit_batch(Vec<ExecutionRequest>) -> Vec<Result<ExecutionId, _>>`. Adding it to the trait now is speculative; adding it later is a minor version bump if additive-default-impl suffices.

**Lean:** defer. Add when a consumer asks.

### §7.5 `progress` merge — is frame-append a top-level op?

**Round-2 status: resolved — split.** K#6 demonstrated the merge was unsound: the `Both` variant had no honest cost model (two FCALLs on Valkey violates §3.4 atomicity; one combined Lua function is unbudgeted in §10). Round-2 splits back into `progress` (percent/message) and `append_frame` (frame bytes) as separate trait methods. §3.1 item 3 + 3b updated. The trait goes from 12 to 13 methods.

### §7.6 `report_usage` split from `complete` — which crate owns it?

**The choice.** Today, `report_usage_and_check` is callable mid-attempt *and* is wrapped into the complete path. The trait splits it: mid-attempt calls go through `report_usage`, terminal ones go through `complete` (which internally may call the backend's usage-settle path). This means there are two slightly-different accounting moments. Is that a consumer confusion point?

**Lean:** clear in practice — "report_usage is mid-attempt only; complete settles terminal accounting." Document in trait rustdoc.

### §7.7 Object-safety constraint: force or leave flexible?

**Round-2 status: closed — force object-safety via concrete types at the boundary.** Round-1 left this open, which K#5 correctly flagged as incompatible with the five associated types used elsewhere. Round-2 resolves in-RFC:

* No associated types. `Handle`, `Error`, `CompletionStream` are concrete (§4).
* `#[async_trait]` (or `trait_variant::make`) is required for `dyn`-safety on async methods; see §7.15.
* Static dispatch (`<B: EngineBackend>`) still works and pays no overhead beyond the concrete-struct Handle parse-on-op cost.
* Dynamic dispatch (`Arc<dyn EngineBackend>`) works without per-use-site type projection.

The round-1 "leave flexible" option is closed. The trait IS object-safe, concretely.

### §7.8 Trait location — `ff-core`, `ff-sdk`, or a new `ff-backend-api` crate?

**The choice.** The trait definition must live somewhere all backend crates + the SDK can depend on. Candidates:
- `ff-core` — already the shared base. But `ff-core` today has no `async` trait dep; adding one pulls tokio into `ff-core`'s build graph.
- `ff-sdk` — but the Valkey backend sits below the SDK in the dep order.
- New `ff-backend-api` crate — clean separation, one more crate to maintain.

**Lean:** new `ff-backend-api` crate. The dep-graph hygiene justifies the extra crate.

### §7.9 Error-projection constraint — enforceable?

**Round-2 status: partially obsolete.** The round-1 framing assumed `B::Error` was an associated type. Round-2 (§4.2) makes `EngineError` the concrete trait-level error — backends classify their native errors into `EngineError` variants *inside the backend implementation* before returning. The classification happens at a known syntactic location (the `?`-chain / explicit conversion inside each impl method); mis-classification is a per-backend code-review concern, not a trait-language-level enforcement concern.

What remains debate-worthy: the `EngineError::Transport(Box<dyn Error + Send + Sync>)` variant carries the raw backend error for cases where the classifier genuinely cannot decide. Backends that abuse this (dump every error into `Transport`) defeat the classification. Prose contract in §4.2 names this; no compile-time enforcement. **Lean:** accept the prose contract; revisit if abuse pattern emerges.

### §7.10 When does `ferriskey::Error` stop leaking through the Valkey backend?

**Round-2 status: resolved at Stage 1.** Round-2 makes `EngineError` the concrete trait error (§4.2); backends wrap transport errors inside `EngineError::Transport(Box<dyn Error + Send + Sync>)`. At the trait boundary, `ferriskey::Error` is already erased behind the Box. Downcasting for diagnostics is a per-backend opt-in concern, not a public-surface concern. Issue #88's "seal `ferriskey::Error`" ask is satisfied by the trait boundary itself; there is no separate sealing step. Closed.

### §7.11 Feature-flag the trait?

**The choice.** During experimentation, put `pub trait EngineBackend` behind `#[cfg(feature = "backend-trait")]` so consumers don't see it until it's stable. Pre-landing safety net.

**Lean:** no feature flag. This is not an experimental feature; it's the architectural keystone. Feature-flagging implies optional; this is not optional post-Stage 1.

### §7.12 How does `AdminBackend` compose with `EngineBackend`?

**The choice.** A backend author writes `impl EngineBackend for ValkeyBackend` and `impl AdminBackend for ValkeyBackend`. A consumer who wants both holds `Arc<ValkeyBackend>` and uses whichever methods they need. For a user who wants a dyn-dispatch multi-trait setup, `trait Backend: EngineBackend + AdminBackend` can be defined in the consumer's crate.

**Lean:** no Backend super-trait in our code; let consumers compose as they wish.

### §7.13 Backend-specific extension points

**The choice.** A backend might want to expose a method that only makes sense on itself (e.g., `ValkeyBackend::run_raw_fcall` for diagnostic tooling). Where does that go?

**Lean:** inherent methods on the concrete `ValkeyBackend` type, accessible via downcast. Not on any trait. Tooling that needs Valkey-specific introspection explicitly pins to `ValkeyBackend`; other consumers are abstraction-pure.

### §7.14 Split `BudgetBackend` from `EngineBackend` for LLM-heavy consumers? (K's deserves-debate #1)

**The choice.** `report_usage` is one trait method today. K argues that LLM workloads report usage 10-100x per attempt (every model call) while claim/progress/complete are once-per-attempt. A `BudgetBackend` split would let budget-heavy consumers hold a dedicated backend with its own pool / batching strategy, without forcing that complexity on the execution hot path.

**Arguments for split.**
* Consumer class is structurally different — budget-heavy apps are a distinct population.
* Batching / connection-pool strategy for budget might differ from execution-ops strategy.
* `report_usage` is the only method with a 10-100x per-attempt frequency; the rest are 1x.

**Arguments for keeping merged.**
* Today's code runs `ff_report_usage_and_check` as one FCALL per report; connection pooling is already shared at the ferriskey-client level.
* A consumer who wants to batch can wrap the trait in a batching adapter client-side without a trait split.
* Extra trait means every LLM-heavy consumer holds two backend refs for what is logically one backend.

**Lean (round-2):** keep `report_usage` on `EngineBackend`. Batching is a consumer-adapter concern. If a real LLM-heavy consumer surfaces and demonstrates that connection-pool or batching needs differ materially, revisit with evidence. Recorded in §8.18 as a defensible position.

### §7.15 `#[async_trait]` vs native `async fn` in traits (K's deserves-debate #4)

**The choice.** Rust 1.75+ supports native `async fn` in traits (for inherent static dispatch). Object-safety of async trait methods currently requires `#[async_trait]` or `trait_variant::make` (which desugars to `Pin<Box<dyn Future>>`). Since §7.7 closed on forcing object-safety, this choice is forced.

**Lean:** `#[async_trait]` for now (established, well-understood; desugar cost is one Box-alloc per call, negligible relative to FCALL round-trip). Revisit in the post-2024-edition-stabilised era when native `async fn` gains object-safety support; the revisit is additive. Recorded in §8.19.

### §7.17 `Bytes` vs `Box<[u8]>` for `Handle.opaque` payload (round-4, M-D1)

**The choice.** §4.1's `Handle` uses `bytes::Bytes`. If the trait lives in `ff-backend-api` (§7.8 lean), that crate pulls the `bytes` crate as a public-type transitive dep. Cheaper alternative: `Box<[u8]>` (no extra dep, no ref-counting, one allocation per Handle) or a `HandleOpaque(Box<[u8]>)` newtype.

**Arguments for `Bytes`:** cheap clone (ref-counted), zero-copy slicing, well-established in the Rust async ecosystem. Backends that hand the same opaque bytes to multiple call sites (e.g., caching the encoded Handle for reuse) pay zero copy cost.

**Arguments for `Box<[u8]>`:** no extra public-type dep; consumers who already use `bytes` elsewhere are unaffected; consumers who don't avoid one transitive. A `Handle` is typically not cloned on the hot path (worker owns it for the attempt duration), so ref-counting is unused.

**Lean (round-4):** `Box<[u8]>` wrapped in a `HandleOpaque` newtype. The transitive-dep cost is real for an architectural-keystone crate; ref-counting is unused; the newtype keeps the public-surface clean and lets us swap to `Bytes` later without a breaking change (additive From impls). Recorded as a minor lean; bikeshed-acceptable at impl time.

### §7.18 Retry-after-transport-error semantics for terminal ops (round-4, M-D2)

**The choice.** `complete(handle, payload)` / `fail(handle, ...)` / `cancel(handle, ...)` consume `Handle` by value. If the call fails with `EngineError::Transport { ... }`, the Handle is moved and gone. On a backend with idempotent replay (§3.4), the caller WANTS to retry the same attempt — but the Handle is dropped.

**Options.**
* (a) **Take `&Handle` for terminal ops too.** Rely on `HandleKind` state tracking inside the backend to reject double-terminal. Runtime `EngineError::State` on the second call. Simple, consistent with non-terminal ops.
* (b) **Reconstruct Handle from SDK-side cache.** The SDK wrapper holds a copy of the `Handle` internally; on transport error, the wrapper reconstructs + retries. Callers never see the move-vs-borrow question.
* (c) **Document the retry story.** Callers who want retry wrap the Handle in `Arc<Handle>` before calling; on transport error they still hold the `Arc` and can call `backend.complete((*arc).clone(), payload)` on retry. Requires `Handle: Clone`.

**Lean (round-4):** (a). Taking `&Handle` for terminal ops uniforms the trait surface with non-terminal ops, relies on the existing `HandleKind` runtime-check discipline (§4.1), and gives caller-side retry for free. The "owned-by-value = consumed" signal the Rust type system offers is cosmetic here because idempotent-replay at the backend is the real guarantee. Update §3.3 sketch: `complete(&self, handle: &Handle, payload)` etc. Recorded as a round-4 revision to the §3.3 signatures; §3.1.1 method 4 / 5 / 6 prose updated accordingly.

*Note:* this is a semantic widening vs. round-2, not a shape change. Existing Valkey-backend's idempotent-replay behaviour already supports `&Handle`; it was the round-2 sketch that artificially moved the Handle.

### §7.19 `ff-server` + Postgres backend for HTTP claim routes (round-4, M-D4)

**The choice.** Today `FlowFabricWorker::claim_via_server` (worker.rs:1014) is an HTTP-orchestration path. Round-2 §7.1 declared this off-trait. With a Postgres backend in play (Stage 2+), ff-server's HTTP claim route must reach a backend somehow.

**Resolution.** `ff-server` holds `Arc<dyn EngineBackend>` and internally calls `backend.claim(...)` on behalf of HTTP callers. The HTTP route becomes a thin shell: parse request → call trait → serialize response. The Postgres-backed ff-server differs from the Valkey-backed ff-server only in what they pass to the `Arc::new(...)` constructor. This is option (a) in M's D4; option (b) (ff-server bypasses the trait) would defeat the entire decoupling.

Recorded explicitly: `ff-server` is a consumer of `EngineBackend`, not a parallel-privileged component. No special routing path exists for HTTP claims.

### §7.20 Admin signal-observation across executions (round-4, M-D5)

**The choice.** `observe_signals(&Handle)` requires a handle — only the current handle-holder can observe. Cairn's bridge-observer pattern and some ops use cases want "observe signals for any execution id" (no handle).

**Resolution.** This is an `AdminBackend` concern, not `EngineBackend`. Add to §3.2's explicit-not-in-trait list: "Cross-execution signal observation (admin scope) lives on `AdminBackend::describe_signals(execution_id)`." The `AdminBackend` trait is not defined in this RFC; the method is noted as part of its future surface so the decoupling is complete.

### §7.16 Postgres-first prototype before ratifying (K's deserves-debate #6)

**The choice.** §5.2 (Stage 2, round-4 — former Stage 3) prototypes Postgres AFTER the trait is ratified. K argues the opposite order is the honest-trait-surface discipline.

**Arguments for Postgres-first.**
* Writing the impl first reveals unstated Valkey assumptions baked into the trait shape.
* An RFC ratified against a working two-backend impl is more trustworthy than one ratified on paper alone.

**Arguments for trait-first.**
* Writing a Postgres impl against an unstable trait means throwing work away every time the trait reshapes in review.
* The trait extraction unblocks #87 + #88 + #93 immediately; Postgres is a distant follow-on.
* Stage 2 (formerly Stage 3, round-4 renumbered) IS the prototype pressure-test; if it surfaces a trait problem, Stage 1 re-opens per the iterative discipline.

**Lean (round-2):** trait-first with Stage 3 as the pressure-test. The inverse-order cost is real (Postgres work thrown away on trait churn); the Stage-3-reopens-Stage-1 escape hatch is the standard RFC-iterative discipline. Recorded in §8.20.

---

## §8 Debate brief — anticipated attacks and responses

Pre-empting the review rounds. RFC-011 §9 established the pattern. Each attack gets a specific response; reviewers who find a hole should dissent with specifics.

### §8.1 "The trait is too ambitious — ship the read-surface trait first and defer writes."

**Response:** The read surface is already shipped (Phase 1, issue #58). The snapshots and `EngineError` are load-bearing and live. This RFC extends the pattern to writes — it is not a novel design, it is the second half of the work Phase 1 proved out. Deferring the write-surface trait means the SDK continues to leak ferriskey on the write path indefinitely; see §1.4 call-site debt argument. The cost of deferring compounds linearly with new SDK methods; the cost of doing now is one focused engineering push.

### §8.2 "10-12 methods is too coarse — a Postgres impl of `complete` will be a monster single method."

**Response:** `complete` wraps a single FCALL today (`ff_complete_execution`) — it is already a "monster single method" in Lua-land, because "complete an execution atomically" is a single atomic operation. A Postgres impl that wraps one SQL transaction matches the Valkey impl's shape 1:1. The method is coarse because the operation is coarse, not because we are hiding complexity. Postgres impl complexity is bounded by the operation's semantic scope, not the trait's method count. If a Postgres author discovers they want to split `complete` internally into 3 SQL statements, that is a Postgres-impl-internal decision — it does not leak to the trait.

### §8.3 "10-12 methods is too fine — `describe_execution` and `describe_flow` should live on a separate read trait."

**Response:** Considered in §6.2. Read-surface already sealed in #58; putting it on a separate trait now means consumers compose two traits for what is conceptually one backend. The ergonomic cost is visible; the structural benefit is invisible (no consumer class needs just-reads-not-writes). The split exists for admin, where the consumer class IS different; it does not exist here.

### §8.4 "`ResumeHandle` separate from `Handle` is over-engineered."

**Response (round-2 CONCEDE — shape changed).** Round-1 kept them distinct for compile-time safety; K#5 showed this was load-bearing on `dyn`-safety which broke for unrelated reasons (associated types). Round-2 collapses all three (`Handle`, `ResumeHandle`, `SuspendToken`) into one concrete `Handle` with a `HandleKind` runtime-checked discriminator (§4.1). The cost analysis shifted: round-1's "distinct types preserve compile-time safety" argument is swamped by the `dyn`-safety gain from collapsing. Runtime check on `kind` per op is microseconds; the generics simplification is substantial. Revised position: single `Handle` is the correct shape. Reviewers who want compile-time phantom-typed `Handle<Fresh>` vs `Handle<Resumed>` can argue it as additive later; it is not needed now.

### §8.5 "`CompletionStream` as an associated type on EngineBackend bloats the trait."

**Response (round-2 MOOT — shape changed per §4.3).** Round-1 declared `type CompletionStream: Stream<...>` as an associated type; round-2 replaces this with `Pin<Box<dyn Stream<Item = CompletionPayload> + Send>>` returned directly from `CompletionBackend` methods (a sibling trait per §90). `EngineBackend` no longer names the stream type. The "bloats the trait" concern is resolved by the associated-type removal; the separate `CompletionBackend` trait remains the natural owner of subscription methods.

### §8.6 "Trait-level error classification enforcement (§7.9) is architectural overreach."

**Response (round-2 MOOT — shape changed).** Round-1 considered a `BackendError::classify()` sub-trait as part of an associated-error-type design. Round-2 makes `EngineError` the concrete trait-level error (§4.2); classification happens inside each backend impl before returning. The sub-trait proposal is unnecessary. Mis-classification remains a per-backend code-review concern; see updated §7.9.

### §8.7 "A Postgres backend implementer will rediscover the trait's shortcomings only at impl time — we should prototype before ratifying."

**Response:** Partial agreement. §5.3 (Stage 3) IS the prototype. We ratify the trait shape at Stage 1 based on Worker F's audit and the mapping in §3.1, then Stage 3 tests the shape against a real Postgres backend, then if Stage 3 surfaces a design mismatch, Stage 1 re-opens. This is the iterative discipline the RFC-011 precedent established. Fully-prototyping-then-RFC would put the RFC after a 2-month Postgres impl effort — we cannot afford that; the trait needs to land to unblock #87, #88, and #93. The Stage 3 validation is the pressure-test.

### §8.8 "Issue #58 already exposed a read surface; why isn't this RFC just 'add the write methods to the existing read trait'?"

**Response:** Issue #58 did not define a read *trait*. It added snapshot types and inherent `describe_*` methods on `FlowFabricWorker`. There is no existing trait to extend; this RFC creates the first such trait and pulls the read methods *onto* it as part of the design. If issue #58 had defined a trait, this RFC would be extending it; but it didn't, and this RFC is the design moment for the trait shape as a whole.

### §8.9 "You said owner decided higher-op granularity; is that decision debate-locked or debate-open?"

**Response:** §1.5.1 — decision-locked. The RFC is written assuming 10-12 methods; the debate rounds do NOT re-litigate granularity. The debate rounds DO litigate the specific choice of methods within the 10-12 budget, the associated-type choices, the naming, and the compositions. Reviewers who believe 18 methods would be better are asked to file a separate RFC rather than block this one.

### §8.10 "Consumer migration shouldn't block; but what if cairn-fabric CI fails against the new trait?"

**Response:** Consumer migration is not a *gate* (§1.5.2). Consumer CI going red is a *signal* — we catch it, diagnose whether it is a trait-design problem or a cairn-adoption problem, and route accordingly. A trait-design problem triggers a trait revision; a cairn-adoption problem stays on the cairn side. The distinction is: does the failure surface because the trait is under-specified (our bug) or because cairn hasn't yet adopted (cairn's timeline)? The Stage 3 prototype helps distinguish these — if the Postgres impl passes and cairn's Valkey-backed impl fails, it's a cairn-migration problem.

### §8.11 "0.3.0 shipping before this trait implies consumers adopt the leaky API then migrate — bad UX."

**Response:** 0.3.0 is already leaky (pre-trait); consumers adopting 0.3.0 are adopting the status quo. The trait extraction in 0.4.x changes internals + adds the trait as a new surface; 0.3.0 consumers migrate to the trait at their own pace. We don't deprecate the inherent-impl surface in 0.4.0 — both coexist. Deprecation lands in 0.5.0 earliest, and removal (of the inherent impl) after consumer migration completes. Consumers never face a "migrate or break" moment; they face a "migrate when ready" moment.

### §8.12 "Why is this RFC-012 and not RFC-013 — doesn't RFC-012 already exist in reference?"

**Response:** RFC-011's §10.3-a text references "RFC-012" as a notional follow-up for the Module API exploration. That text predates the numbering assignment here. This RFC takes RFC-012; if the Module API exploration ever files its own RFC, it takes RFC-013 or later. The reference in RFC-011 was prospective, not reserving.

### §8.13 "Trait methods taking `&self` force interior mutability in the backend — is that OK?"

**Response:** Valkey backends use ferriskey's `Client` which is `&self`-callable. Postgres backends using `deadpool-postgres` or `bb8` have pool handles that are `&self`-callable. Backend-internal state (connection pool, in-flight request registry, etc.) lives behind `Arc<Mutex>` / `Arc<RwLock>` as the pool implementations already do. `&self` is the idiomatic Rust choice for services with internal pool state; `&mut self` would prevent concurrent calls which is exactly what a worker-serving backend cannot do. No issue.

### §8.14 "A future backend implementer might not know the safety properties (§3.4)."

**Response:** The trait rustdoc (implemented alongside Stage 1) names the safety properties per method. A Postgres or other impl that violates them fails the `ff-test` integration suite at Stage 3+ (each property has at least one test that catches a violation — fence-triple mismatch tests, idempotent replay tests, atomicity tests; these already exist for the Valkey impl and are backend-portable because they run through the trait). The trait rustdoc is the documentation layer; the `ff-test` suite is the enforcement layer.

### §8.15 "Does this interact with RFC-011's partition model?"

**Response:** Yes but minimally. RFC-011 established hash-tag co-location so atomic multi-key FCALLs span flow + exec. The trait's Valkey impl relies on this co-location for `complete` / `cancel_flow` atomicity. RFC-011's `execution_id` shape (`{fp:N}:<uuid>`) is an input to `Handle`'s Valkey-impl internals. Post-trait, the partition structure is still observable via `describe_execution` (`ExecutionSnapshot` carries the partition) but is not surface-exposed on non-Valkey backends (Postgres returns `None` for the partition field or returns a backend-specific string-ified row-location; RFC-011 doesn't prescribe).

### §8.16 "A Postgres backend has different retry semantics than Valkey — how does `fail`'s `FailOutcome` abstract that?"

**Response:** `FailOutcome`'s variants (retry-scheduled with delay, retry-exhausted, terminal-failure) describe the decision; backends fulfil the decision in their native way. Valkey: `ff_fail_execution` runs the retry table + delay-calc + FCALL-pipelined ZADD. Postgres: single transaction inserts the retry row + schedules the next-attempt-at timestamp. The trait's `FailOutcome` is the abstract — "here is what the system decided" — decoupled from how that was physically persisted.

### §8.17 "`cancel_flow` / `describe_flow` are admin-shaped; they belong on `AdminBackend`, not `EngineBackend`." (K's deserves-debate #2 + #3)

**Response (round-2 DEFEND).** `cancel_flow` and `describe_flow` share the atomicity / fence / error-classification contract of the rest of the trait and map to single backend round-trips. `AdminBackend` is where ops with a categorically different consumer class live (operator-only tools, key rotation, HMAC rotation). Flow-level describe and cancel are callable from any workflow code that orchestrates sub-flows; they are not operator-only. Splitting them to `AdminBackend` forces workflow code that cares about sub-flow lifecycle to hold two trait refs, paying an ergonomic cost for a structural organisation the consumer doesn't care about. The §6.2 rule — one trait per consumer class, split only when classes genuinely differ — says keep them here.

The K objection (workers don't claim flows; `cancel_flow` is a control-plane op) is structurally correct but doesn't bear the weight to justify a trait split. A worker COULD hold a `ff-backend-valkey` ref and call `cancel_flow` on a sub-flow it spawned; that is a legitimate workflow pattern, not an admin-operator action.

### §8.18 "Split `BudgetBackend` from `EngineBackend` for LLM-heavy consumers." (K's deserves-debate #1)

**Response (round-2 DEFER).** See §7.14. Keeping `report_usage` on `EngineBackend` for now. Batching is a consumer-adapter concern, implementable client-side against the current trait shape. A real LLM-heavy consumer surfacing with a demonstrated connection-pool or batching need — backed by evidence, not speculation — is the condition for revisiting. Today there is no such consumer; splitting on a hypothetical class is speculative abstraction.

### §8.19 "`async_trait` macro vs native `async fn` in traits is unaddressed." (K's deserves-debate #4)

**Response (round-2 CONCEDE — added to §7.15).** K correctly noted §7 had 13 open questions and this wasn't one. Round-2 adds §7.15 explicitly. The choice is forced by §7.7's object-safety decision: `#[async_trait]` for now. Additive swap to native async-fn-in-traits if / when they gain object-safety. The addition is an §7 entry, not an RFC-shape change.

### §8.21 "`Bytes` as Handle payload adds a public-type transitive dep." (M's D1)

**Response (round-4 CONCEDE).** Switch to `Box<[u8]>` wrapped in a `HandleOpaque` newtype. See §7.17. The transitive-dep cost for an architectural-keystone crate is real; ref-counting is unused on the hot path. Additive From impls let us swap to `Bytes` later without a breaking change.

### §8.22 "Terminal ops consume Handle by value — retry-after-transport-error story broken." (M's D2)

**Response (round-4 CONCEDE).** Switch terminal ops to `&Handle` per §7.18 option (a). The move-by-value gave cosmetic type-system-level "consumed" signalling; idempotent-replay at the backend is the real guarantee and is already preserved. §3.3 sketch updated accordingly in the impl PRs.

### §8.23 "`claim_via_server` off-trait leaves ff-server + Postgres path undefined." (M's D4)

**Response (round-4 RESOLVE).** `ff-server` holds `Arc<dyn EngineBackend>` and calls `backend.claim(...)` from its HTTP route. Same trait path for Valkey-backed and Postgres-backed ff-server deployments. See §7.19. ff-server is a consumer of the trait, not a trait-bypassing component.

### §8.20 "Postgres-first prototype before ratifying the trait." (K's deserves-debate #6)

**Response (round-2 DEFEND — added to §7.16).** See §7.16 for the full argument. Summary: Stage 2 (formerly Stage 3, round-4 renumbered) IS the prototype pressure-test; the inverse order costs Postgres impl work thrown away on trait churn; the Stage-3-reopens-Stage-1 escape hatch is the iterative discipline we already use for RFC-011-series work.

---

## §9 Sequencing with other issues (#87, #88, #90, #91, #92, #93)

The follow-up issues all trace back to this RFC. Sequencing:

**#87 (ferriskey::Client leak on FlowFabricWorker).** Collapses to "consumers interact with `dyn EngineBackend` or `<B: EngineBackend>`; ferriskey::Client becomes a `ValkeyBackend`-private field." Lands with Stage 1.

**#88 (ferriskey::Error leak on SdkError).** Collapses at the trait boundary via the broadened `EngineError::Transport { backend, source: Box<dyn Error + Send + Sync> }` (Stage 0). ferriskey::Error is erased behind the Box. Lands with Stage 0 (broadening) + Stage 1 (trait).

**#90 (CompletionStream trait replacing ff:dag:completions pubsub).** This RFC authorises the `CompletionStream` shape (§4.3) and names `CompletionPayload` as a Stage-0 new type (§3.3.0). The actual `CompletionBackend` trait definition lives in issue #90's deliverable, informed by this RFC's return-type choices.

**#91 (opaque PartitionKey on wire types).** Parallel-independent work. `ClaimGrant.partition: String` → `ClaimGrant.partition: PartitionKey` (opaque newtype) is orthogonal to the trait extraction. Sequence before or after Stage 1; no dependency either way.

**#92 (StreamCursor enum).** Separate `StreamBackend` trait per §6.2. Post-Stage-1 work.

**#93 (BackendConfig).** FUSED into Stage 1 per round-4 M6. The former "Stage 2: BackendConfig" is no longer a separate stage; `BackendConfig` lands alongside the trait in one coherent release.

---

## §10 Cost totals (sketch)

A full hour-level breakdown follows the Stage 1 detailed design PR (pre-Stage-1). Rough envelope:

| Stage | Owner           | Scope                                                                                  | Est.       |
|-------|-----------------|----------------------------------------------------------------------------------------|------------|
| 0     | single-agent    | 13 new types + `EngineError` Transport broadening + `Unavailable` variant + `ResumeSignal` move | 6-10h      |
| 1     | single-agent    | Trait definition + `ValkeyBackend` impl + `BackendConfig` + SDK rewire + tests (fused) | 25-35h     |
| 2     | cross-cutting   | `ff-backend-postgres` experimental crate + Postgres CI                                 | 20-30h     |
| 3     | cairn team      | Cairn migration (external)                                                             | cairn-side |
| 4     | single-agent    | Seal `ff_core::keys` + final consumer-check audit                                      | 2-4h       |
| **Total (internal)** | | ~55-80h over 2-3 weeks wall-clock, non-blocking with other work                        | |

The estimate is order-of-magnitude, not hour-exact. The Stage 1 PR cluster is the critical chunk; Stage 2+ are parallelisable with cairn-independent work.

---

## §11 References

* RFC-001 (Execution lifecycle) — trait methods `claim`/`complete`/`fail`/`cancel` preserve the RFC-001 semantics layer.
* RFC-003 (Lease) — `renew` + fence-triple (§3.4) preserve RFC-003 semantics.
* RFC-004 (Suspension) — `suspend` / `resume` + `SuspendToken` preserve RFC-004 semantics.
* RFC-007 (Flow) — `describe_flow` / `cancel_flow` preserve RFC-007 semantics.
* RFC-008 (Budget) — `report_usage` preserves RFC-008's admission + usage model.
* RFC-010 (Valkey architecture) — `ValkeyBackend` implements per RFC-010's partition model.
* RFC-011 (Exec/flow co-location) — `Handle`'s Valkey internals rely on RFC-011's hash-tagged exec-id shape.
* Issue #58 (Engine-level primitives) — Phase 1 read surface + `EngineError`; this RFC is Phase 2.
* Issue #87 (ferriskey::Client leak) — closed by Stage 1.
* Issue #88 (ferriskey::Error leak) — closed by Stage 1.
* Issue #89 (ExecutionOperations trait) — this RFC IS the design for #89.
* Issue #90 (CompletionStream trait) — authorised by this RFC; detailed design #90.
* Issue #91 (opaque PartitionKey) — orthogonal; sequenced around Stage 1.
* Issue #92 (StreamCursor enum) — separate `StreamBackend` trait; post-Stage-1.
* Issue #93 (BackendConfig) — Stage 2 of this RFC.
* Worker F audit (2026-04-22) — leak inventory cited in §1.2.
