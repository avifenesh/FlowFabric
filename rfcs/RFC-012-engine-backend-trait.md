# RFC-012: `EngineBackend` trait — abstracting FlowFabric's write surface

**Status:** Draft — round 2 (post Worker K CHALLENGE; round-2 revisions applied).
**Created:** 2026-04-22
**Supersedes:** issue #89 (trait-extraction plan) on acceptance.
**Related:** issues #87, #88, #90, #91, #92, #93 (concrete follow-up work this RFC authorises).
**Predecessor:** issue #58 Phase 1 — sealed the read surface (`ExecutionSnapshot`, `FlowSnapshot`, `EdgeSnapshot`) and typed the error surface (`EngineError` — landed on `main` at `crates/ff-sdk/src/engine_error.rs` per PR #81; round-2 verified on `25b2aad`).

### Round-2 revision summary

Worker K's first-round CHALLENGE landed 8 must-fix items; the orchestrator corrected #1 (K read a stale checkout — `EngineError` does exist in `crates/ff-sdk/src/engine_error.rs`). Items #2–#8 drove the following RFC revisions, applied in-place:

* **#2 (resume atomicity fusion):** split `resume` into `observe_signals` + `claim_from_reclaim` (§3.1 item 8, §3.4).
* **#3 (claim combined + reclaim third path):** kept `claim` for fresh work; reclaim moves to `claim_from_reclaim`; `claim_via_server` is explicitly an HTTP-orchestration concern, not a trait method (§3.1 item 1, §7.1).
* **#4 (§3.4 atomicity overpromise):** added §3.4.1 distinguishing commit atomicity from notification atomicity; contract weakened for cross-entity side-effects on backends without atomic outbox.
* **#5 (object-safety + associated-types contradiction):** §7.7 resolved in-RFC. Dropped five associated types; `Handle`/`ResumeHandle`/`SuspendToken` collapse to an opaque concrete struct; `Error` is the concrete `EngineError`; `CompletionStream` is `Pin<Box<dyn Stream<Item = CompletionPayload> + Send>>`. §3.3 sketch rewritten; §4 rewritten; §6.4 rewritten.
* **#6 (`progress` Both-variant):** split back into `progress` (percent/message) + `append_frame`. `Both` variant deleted. Trait goes to 13 methods (§3.1 item 3, §7.5).
* **#7 (Stage 3 `unimplemented!()`):** replaced with `Err(EngineError::Unavailable { op })`. Requires adding the `Unavailable` variant to `EngineError` as Phase-1 prerequisite, tracked in §5.3 (§5.3 rewritten).
* **#8 (Stage 5 seal + cairn):** reframed per owner's cairn-no-gate decision. Internal `ff-*` grep is a gate; cairn's hot-path decoupling is not (§5.5 rewritten).

K's six "deserves debate" items are addressed in §7 (new entries §7.14–§7.16) and §8 (new entries §8.17–§8.19).

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
* **Not a re-design of the execution model.** The trait mirrors the existing semantics — claim / progress / complete / fail / cancel / suspend / resume — just typed. RFCs 001-009 define those semantics; this RFC adds a dispatch seam, not a new semantics layer. A reviewer who finds a semantic mismatch between the trait and the existing RFCs should flag a bug in the trait, not propose a semantics change.
* **Not a replacement of FCALLs inside the Valkey impl.** The 18 Lua functions continue to exist. They are how the Valkey backend fulfils the trait. A single trait method may drive one or several FCALLs; the point is that the FCALL names and KEYS/ARGV layouts are backend-internal once the trait lands. They do not appear in the public SDK surface.
* **Not a new admin API surface.** Admin operations (waitpoint HMAC rotation, partition collision diagnostics, cutover runbook tooling) live on a separate trait or sit on the backend impl as inherent methods. §3.2 names the ones that *don't* go on `EngineBackend`.
* **Not a synchronous-API proposal.** The trait is `async` throughout. The existing SDK is async; a blocking variant (if ever wanted) is a follow-up trait, not a parallel definition here.
* **Not a multi-tenancy / capability-routing redesign.** RFC-009's capability routing sits above the trait. `claim` takes a capability set; how the backend satisfies routing is backend-internal. Changes to the routing model itself belong in a follow-up RFC.

---

## §3 Proposed `EngineBackend` trait shape

### §3.1 Operation inventory (13 methods — round-2 revised)

The owner's decision pins the granularity target at ~10-12 business-operation methods; round-2 revisions (K#2 split of `resume`, K#6 split of `progress`) brought the count to 13. The owner's "~10-12" is a range, not a hard ceiling; 13 is within the spirit of the decision (the `progress`/`append_frame` split replaces a fused method whose atomicity was unsound, and the `observe_signals`/`claim_from_reclaim` split replaces one method whose multi-round-trip honesty was broken). Below is the round-2 inventory.

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

**4. `complete(handle, payload) -> Result<(), BackendError>`.** Terminal success. Consumes the handle. Payload is the attempt's result bytes (caller-chosen encoding; trait doesn't mandate JSON). On Valkey, complete runs a single 12-KEY FCALL that mutates exec state, flow-membership terminal set, unblocks children, and publishes a completion event. On Postgres, a single transaction mutates `executions`, `flow_memberships`, `child_dependencies`, and inserts into `completion_events`.

*Maps to:* `ff_complete_execution`.

**5. `fail(handle, reason, classification) -> Result<FailOutcome, BackendError>`.** Terminal failure (possibly with retry scheduling). `FailOutcome` carries whether the failure triggered a retry (with the new attempt's delay) or was final. The retry-decision policy is backend-internal — retry table, budget admission, and delay calculation all live inside the FCALL on Valkey and the transaction on Postgres. Caller sees the outcome.

*Maps to:* `ff_fail_execution`. Includes the retry-scheduling sub-branches today visible as Lua status codes.

**6. `cancel(handle, reason) -> Result<(), BackendError>`.** Terminal cancellation. Consumes the handle. Reason is an opaque caller string for observability; trait does not constrain its vocabulary.

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

**Stream reads (XRANGE / XREAD).** Stream access is a related-but-distinct decoupling issue (#92). Stream cursors are a different abstraction (they need a cursor type; XRANGE markers don't map to row-based backends without a translation layer). A separate `StreamBackend` trait owns the stream surface. Callers who want both hold two trait objects or a composite. This RFC names StreamBackend's existence; its shape lives in the #92 follow-up RFC.

**Completion pubsub subscription.** Issue #90 files a `CompletionStream` trait. This RFC folds the trait's return-types into `EngineBackend`'s shape — specifically, any trait method that today returns a "subscribe to completions for this entity" side-effect returns a `CompletionStream` typed by the associated type (§4). The stream type itself is this RFC's responsibility (it shapes return types); the subscription mechanism for bulk-tailing the completion channel is #90's trait.

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
    kind: HandleKind,         // { Fresh, Resumed, Suspended } — checked on every op call
    opaque: Bytes,             // backend-private state (lease token, row id, etc.)
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
    ) -> Result<(), EngineError>;

    async fn complete(
        &self,
        handle: Handle,
        payload: Option<Bytes>,
    ) -> Result<(), EngineError>;

    async fn fail(
        &self,
        handle: Handle,
        reason: FailureReason,
        classification: FailureClass,
    ) -> Result<FailOutcome, EngineError>;

    async fn cancel(
        &self,
        handle: Handle,
        reason: &str,
    ) -> Result<(), EngineError>;

    async fn suspend(
        &self,
        handle: Handle,
        waitpoints: Vec<WaitpointSpec>,
        timeout: Option<Duration>,
    ) -> Result<Handle, EngineError>;   // returns a fresh Handle with kind=Suspended

    async fn observe_signals(
        &self,
        handle: &Handle,
    ) -> Result<Vec<ResumeSignal>, EngineError>;

    async fn claim_from_reclaim(
        &self,
        token: ReclaimToken,
    ) -> Result<Option<Handle>, EngineError>; // kind=Resumed on Some

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
    ) -> Result<AdmissionDecision, EngineError>;
}
```

The trait is both object-safe (`dyn EngineBackend` works; no associated types to name) and usable with generics (`<B: EngineBackend>`). Both dispatch styles are available to consumers without trait-level reshape. `#[async_trait]` is required for `dyn`-safety on async methods pre-Rust-2024-trait-dyn-async (§7.15 below addresses this choice).

### §3.4 Safety properties preserved

Every trait method must preserve the same fence / idempotency / atomicity properties the current inherent implementation enforces. The trait contract names them; the Valkey backend's current behaviour fulfils them by construction (the Lua functions are already written that way); a future Postgres backend must also fulfil them.

**Fence triple**: every op that mutates attempt state requires `(lease_id, lease_epoch, attempt_id)`. The handle carries these internally; the backend enforces them on every op call. On Valkey, fence-triple checks happen inside the Lua function via keys/args. On Postgres, fence-triple checks happen via `WHERE lease_id = $1 AND lease_epoch = $2 AND attempt_id = $3` in the UPDATE clause.

**Idempotent replay**: `complete`, `fail`, `cancel`, `suspend` are idempotent under replay — calling twice with the same handle and args returns the same result (success on first call, "already in terminal state" on subsequent calls). The trait contract mandates this. Callers can safely retry under network uncertainty.

**Atomicity**: per-op state transitions are atomic — partial state is not observable. On Valkey this is the single-FCALL-per-op property (RFC-011 §5.5 closed the last cross-partition gap). On Postgres this is the per-transaction property. The trait contract names the atomicity requirement; a backend that can't honour it (e.g., a NATS-backed design that can't promise cross-subject atomicity) either must not implement `EngineBackend` or must degrade specific ops with a documented contract deviation.

*Exception:* `observe_signals` (method 8) explicitly does NOT offer atomicity — it is a pure read with potentially multiple round-trips on Valkey (HGETALL + per-waitpoint HMGET). Because no state is mutated, atomicity is not semantically meaningful. Listed here for completeness.

**Monotonic lease epoch**: lease epoch only advances. No op moves it backward. The backend enforces; the trait contract names the property for consumer-side correctness proofs.

### §3.4.1 Commit atomicity vs. notification atomicity (round-2, K#4)

Round-1 §3.4 asserted "per-op state transitions are atomic" without distinguishing the state-transition commit from downstream side-effect delivery. For single-entity ops (e.g., `renew`, `progress`), the distinction is immaterial. For cross-entity ops (`complete`, `fail`, `cancel`, `cancel_flow`, `suspend`, `claim_from_reclaim`), the distinction is load-bearing. `complete` in particular mutates `executions`, flow-membership terminal set, child-dependency edges, and publishes a completion event. Valkey covers all four within one FCALL; Postgres cannot cover the NOTIFY publication within the row-commit transaction (NOTIFY delivers post-commit).

Round-2 contract distinguishes two atomicity classes:

**Commit atomicity (required of every backend that impls `EngineBackend`).** The backend's persistent state transition is atomic with respect to itself. Either the complete/fail/cancel/cancel_flow transition is fully committed or it is not observable. On Valkey: one FCALL. On Postgres: one transaction spanning all four entity tables. On any other backend: equivalent per-op single-transaction semantics. A backend that cannot guarantee commit atomicity for these cross-entity ops does NOT implement `EngineBackend`; it either composes partial backends or ships with an explicit contract deviation.

**Notification atomicity (NOT guaranteed across backends).** Downstream subscribers to completion events observe the event in their own time. Valkey gives near-atomicity (pubsub is best-effort in-process; subscribers receive shortly after commit). Postgres gives notify-after-commit (NOTIFY fires at `COMMIT` time, strictly post-commit; there is a window where the row is visible to other readers but the notification has not yet reached subscribers). Other backends may have stronger or weaker guarantees.

**Consumer-side contract asymmetry.**
* Consumers relying on "the completion event arrived → therefore the execution is committed" MAY assume the implication (notify-after-commit is the guarantee). Downstream state observation after notification is safe.
* Consumers relying on "the execution is committed → therefore the notification arrived" MUST NOT assume the implication. The notification may not yet have reached them. Any such consumer must tolerate a visibility window and either poll on `describe_execution` for confirmation or accept eventual-consistency.

**Backend-side contract.** A backend's `complete` (and siblings) implementation MUST ensure the notification is scheduled within or after the commit — never before (never-pre-commit is the invariant that makes the consumer-side "notification implies commit" assumption safe). Valkey satisfies this trivially: the FCALL's PUBLISH is in the same Lua execution as the state mutation. Postgres satisfies this via `NOTIFY` inside the transaction (NOTIFY deliveries are deferred to commit; rollback cancels queued notifies).

**Outbox pattern (optional).** A backend that wants to strengthen to "notification-eventually-arrives given commit" (rather than notify-best-effort) is free to add an outbox table + publisher process. The trait contract does not require this; naïve fire-and-forget PUBLISH on Valkey is within contract, and plain NOTIFY on Postgres is within contract.

**Why this matters 6 months out.** A cairn or external consumer writing logic of the form "I saw complete() return; therefore my peer-observer has been woken" will be broken on Postgres if we don't make the asymmetry explicit. The fix is not a weaker `complete` (it's atomic at commit) but an explicit named contract the consumer can code against.

---

## §4 Concrete trait types (round-2 — no associated types)

Round-1 used five associated types; round-2 replaces all five with concrete types at the trait boundary (K#5 resolution — §7.7 closed). The change makes the trait object-safe without requiring per-use-site type projection and eliminates the generics-proliferation cost across SDK wrapper signatures.

### §4.1 `Handle` — concrete opaque struct

```text
pub struct Handle {
    backend: BackendTag,
    kind: HandleKind,          // Fresh | Resumed | Suspended
    opaque: bytes::Bytes,      // backend-private state
}
```

Owned by the worker for the duration of the attempt. Consumed by terminal ops (`complete`, `fail`, `cancel`); transformed by `suspend` (returns a fresh Handle with kind=Suspended); borrowed by non-terminal ops (`renew`, `progress`, `append_frame`, `observe_signals`, `report_usage`).

**Valkey impl:** the `opaque` bytes encode exec id, attempt id, lease id, lease epoch, capability binding, partition. Decode is backend-internal.

**Postgres impl:** `opaque` encodes row primary key + lease token + fence-triple.

**Why concrete, not opaque-associated-type:** associated types cannot be named through `dyn EngineBackend` without a per-use-site projection. A concrete struct with an internal opaque-bytes payload gives the same API-surface hiding (consumer never constructs a Handle; backend is the only writer) while remaining dyn-safe. Backends pay a serialize-on-create + parse-on-op cost; on Valkey this is microsecond-range (a small struct-pack), dominated by the FCALL round-trip.

**`HandleKind` runtime checking.** Round-1 used compile-time type distinctness (`ResumeHandle` vs `Handle`) to prevent threading a resumed handle into a path expecting a fresh one. Round-2 makes this a runtime check: the backend validates `kind` on entry to each op and returns `EngineError::State { expected, actual }` on mismatch. This matches the existing fence-triple runtime-check discipline — which, unlike the handle-kind split, we never proposed to make compile-time-enforced, because the runtime check is already cheap and the value is clear. (If reviewers revisit the cost/benefit and prefer the compile-time enum with a `Handle<Fresh>` / `Handle<Resumed>` phantom-typed approach, that's an additive change later; round-2 picks the simpler shape first.)

### §4.2 `EngineError` — concrete, Phase-1 landed

Backends return the concrete `EngineError` defined in `crates/ff-sdk/src/engine_error.rs` (Phase 1 landed on `main` at `25b2aad`). Variants:

* `Validation` — input violates invariants (non-retryable)
* `Contention(ContentionKind)` — caller should retry with backoff
* `Conflict(ConflictKind)` — another caller won a race; caller decides whether to retry
* `State(StateKind)` — the targeted entity is in the wrong state (e.g., already terminal)
* `Bug(BugKind)` — internal inconsistency; file a bug
* `Transport(Box<dyn Error + Send + Sync>)` — backend-specific connectivity / protocol failure
* `Unavailable { op: &'static str }` — round-2 addition (K#7); op is declared but not yet implemented on this backend

The `Transport` variant is where backend-private errors (`ferriskey::Error`, `sqlx::Error`, etc.) are carried. They are boxed-and-erased at the trait boundary so the trait itself depends on nothing backend-specific. Consumers wanting to introspect the transport-layer error for backend-specific diagnostics must downcast; ordinary flow-control uses the structural variants.

**Round-2 change (K#7):** the `Unavailable` variant is new. It was already logically needed for staged backend impls (§5.3); round-1 specified `unimplemented!()` which is a panic, not an error. Round-2 replaces all `unimplemented!()` in the Stage 3 Postgres skeleton with `Err(EngineError::Unavailable { op: "<name>" })`. This variant addition is a Phase-1-scope minor change (additive enum variant) tracked as a prerequisite for Stage 3.

**Classification invariant.** Every backend must map its native errors into these structural variants deterministically. Valkey: `ScriptError` + `ferriskey::ErrorKind` classify per the existing `classify()` function (Phase 1). Postgres: `sqlx::Error` / `tokio_postgres::Error` classify per a new per-crate classifier (part of the Postgres-backend RFC). The invariant is a prose contract — Rust has no language-level constraint to enforce it. §7.9 debates enforcement via a `BackendError::classify()` sub-trait method; since `EngineError` is now the concrete trait-level error, classification happens inside the backend before returning, not via a sub-trait method. §7.9 is therefore partially obsolete post-round-2 (see updated §7.9).

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

## §5 Migration plan

The trait extraction is staged. Each stage is independently landable, with an acceptance gate and rollback posture. No single PR attempts the full migration.

### §5.1 Stage 1 — Trait extraction, Valkey impl stays put (this RFC accepted)

**Scope.** Define `EngineBackend` trait in a new module `ff_core::backend` (or `ff_sdk::backend` — location decision per §7). The current `FlowFabricWorker` + `ClaimedTask` inherent impls become a single `ValkeyBackend` implementation of the trait, living in a new crate `ff-backend-valkey` (or as a module in `ff-sdk` if the crate-split is deferred; §7.1).

**What does not change:** Public SDK API. Consumers continue calling `ClaimedTask::complete(self, payload)`. The inherent impl becomes a thin forwarder to `ValkeyBackend::complete(handle, payload)`. Zero behaviour change. Zero wire change.

**What does change:** The internal structure. A new trait exists; the Valkey backend exists as a named type implementing it. The existing SDK code rewires to use the trait internally. Downstream consumers are unaware.

**Landing gate.** `cargo test --workspace` green. `ff-test` green against a real Valkey. No public-surface changes flagged by `cargo semver-checks` (if configured) beyond additive.

**Rollback.** Trait-extraction is a refactor; rollback is a revert. No user-observable shape to preserve.

### §5.2 Stage 2 — `BackendConfig` replaces `WorkerConfig`'s connection primitives (issue #93)

**Scope.** `WorkerConfig`'s `host`, `port`, `tls`, `cluster` fields move behind a `BackendConfig` trait (or struct — tbd at impl time). Worker-policy fields (`lanes`, `capabilities`, `lease_ttl_ms`, etc.) stay on `WorkerConfig`. `WorkerConfig::connect()` becomes `WorkerConfig::connect_with(backend)` taking a pre-constructed backend handle.

**Migration shim.** `WorkerConfig::new(host, port, ...)` continues to exist as a convenience constructor on the Valkey path, internally building a `ValkeyBackendConfig` and calling `connect_with`. Old-call-site code compiles unchanged. New call sites (multi-backend consumers) use the explicit form.

**Landing gate.** `ff-test` green. Example apps (`examples/media-pipeline`, `examples/coding-agent`) green. Cairn-fabric's CI green against the pre-0.4 release.

### §5.3 Stage 3 — Experimental `ff-backend-postgres` crate

**Prerequisite.** The `EngineError::Unavailable { op: &'static str }` variant (§4.2) lands as a Phase-1-scope PR before Stage 3 begins. This is a ~10-line additive enum-variant change to `crates/ff-sdk/src/engine_error.rs`; it does not break existing consumers.

**Scope.** A new crate `ff-backend-postgres` lands with a minimal `EngineBackend` impl. Minimum-surface: `claim` / `renew` / `progress` / `append_frame` / `complete` / `fail` / `describe_execution`. Unimplemented methods return `Err(EngineError::Unavailable { op: "<method_name>" })` — NOT `unimplemented!()`.

*Round-2 note (K#7):* round-1 said unimplemented methods "remain `unimplemented!()`." `unimplemented!()` panics, which in an async runtime propagates to the task and, for a library crate, gives a consumer evaluating the crate a process-level panic with no recovery path. Replacing with `Err(EngineError::Unavailable { op })` gives graceful degradation: consumers receive a typed error they can match on and fall back (or report clearly to the user). The booby-trap is removed.

**This is not a shippable Postgres backend.** It is a proof-of-concept that the trait shape is implementable against a non-Valkey backend. Its existence validates that the Stage 1 trait design did not accidentally bake in Valkey-specific assumptions; if it did, Stage 1 reopens.

**Landing gate.** Integration test suite against a Postgres 15 container for the implemented method set. `Unavailable` returns are exercised once each to confirm they do not panic.

**Non-ship contract.** The crate ships under `[features]` or with a `README` that says "experimental; non-production." No version promise.

### §5.4 Stage 4 — Cairn-fabric migration (cairn-team timeline)

Cairn migrates at their own pace. Our side: keep the `EngineBackend` trait stable (no breaking changes post-Stage 1 without a minor version bump), provide the `ff-backend-valkey` crate, provide the migration guide. Cairn's team decides when to adopt.

No gate on our side. This stage completes when cairn files a PR closing their migration.

### §5.5 Stage 5 — Seal `ff_core::keys` as `pub(crate)`

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

### §6.5 Shared implementations across backends via a base trait

**Rejected as premature.** A hierarchy like `trait Backend { ... default impls ... }` + `trait EngineBackend: Backend` sharing commonalities across future backend impls (e.g., fence-triple verification logic) is worth considering once a second backend exists. Today, exactly one backend exists (Valkey); the default-impl slots would all be `unimplemented!()` placeholders. Deferred to post-Stage-3 when the Postgres backend has materialised and shared surfaces become visible.

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

### §7.16 Postgres-first prototype before ratifying (K's deserves-debate #6)

**The choice.** §5.3 (Stage 3) prototypes Postgres AFTER the trait is ratified. K argues the opposite order is the honest-trait-surface discipline.

**Arguments for Postgres-first.**
* Writing the impl first reveals unstated Valkey assumptions baked into the trait shape.
* An RFC ratified against a working two-backend impl is more trustworthy than one ratified on paper alone.

**Arguments for trait-first.**
* Writing a Postgres impl against an unstable trait means throwing work away every time the trait reshapes in review.
* The trait extraction unblocks #87 + #88 + #93 immediately; Postgres is a distant follow-on.
* Stage 3 IS the prototype pressure-test; if it surfaces a trait problem, Stage 1 re-opens per the iterative discipline.

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

### §8.20 "Postgres-first prototype before ratifying the trait." (K's deserves-debate #6)

**Response (round-2 DEFEND — added to §7.16).** See §7.16 for the full argument. Summary: Stage 3 IS the prototype pressure-test; the inverse order costs Postgres impl work thrown away on trait churn; the Stage-3-reopens-Stage-1 escape hatch is the iterative discipline we already use for RFC-011-series work.

---

## §9 Sequencing with other issues (#87, #88, #90, #91, #92, #93)

The follow-up issues all trace back to this RFC. Sequencing:

**#87 (ferriskey::Client leak on FlowFabricWorker).** Collapses to "consumers interact with `dyn EngineBackend` or `<B: EngineBackend>`; ferriskey::Client becomes a `ValkeyBackend`-private field." Lands with Stage 1.

**#88 (ferriskey::Error leak on SdkError).** Collapses to `B::Error` (per §4.2). Lands with Stage 1.

**#90 (CompletionStream trait replacing ff:dag:completions pubsub).** This RFC authorises the `CompletionStream` associated type (§4.4); the actual `CompletionBackend` trait definition + wire shape is issue #90's deliverable, landing in Stage 1 or Stage 2 depending on complexity. The shape is informed by this RFC's return-type choices.

**#91 (opaque PartitionKey on wire types).** Parallel-independent work. `ClaimGrant.partition: String` → `ClaimGrant.partition: PartitionKey` (opaque newtype) is orthogonal to the trait extraction. Sequence before or after Stage 1; no dependency either way. Landing post-Stage-1 is slightly nicer because `ClaimGrant` then lives more fully inside the backend's type vocabulary.

**#92 (StreamCursor enum).** Separate `StreamBackend` trait per §6.2. Post-Stage-1 work.

**#93 (BackendConfig).** Stage 2 in this RFC's staging. Depends on Stage 1.

---

## §10 Cost totals (sketch)

A full hour-level breakdown follows the Stage 1 detailed design PR (pre-Stage-1). Rough envelope:

| Stage | Owner           | Scope                                                             | Est.       |
|-------|-----------------|-------------------------------------------------------------------|------------|
| 1     | single-agent    | Trait definition + `ValkeyBackend` impl + SDK rewire + tests      | 20-30h     |
| 2     | single-agent    | `BackendConfig` split + shim + example updates                    | 6-10h      |
| 3     | cross-cutting   | `ff-backend-postgres` experimental crate + Postgres CI            | 20-30h     |
| 4     | cairn team      | Cairn migration (external)                                        | cairn-side |
| 5     | single-agent    | Seal `ff_core::keys` + final consumer-check audit                 | 2-4h       |
| **Total (internal)** | | ~50-75h over 2-3 weeks wall-clock, non-blocking with other work | |

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
