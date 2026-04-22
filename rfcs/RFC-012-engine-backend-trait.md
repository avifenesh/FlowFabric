# RFC-012: `EngineBackend` trait — abstracting FlowFabric's write surface

**Status:** Draft — for owner + team debate.
**Created:** 2026-04-22
**Supersedes:** issue #89 (trait-extraction plan) on acceptance.
**Related:** issues #87, #88, #90, #91, #92, #93 (concrete follow-up work this RFC authorises).
**Predecessor:** issue #58 Phase 1 — sealed the read surface (`ExecutionSnapshot`, `FlowSnapshot`, `EdgeSnapshot`) and typed the error surface (`EngineError`).

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

### §3.1 Operation inventory (12 methods)

The owner's decision pins the granularity target at ~10-12 business-operation methods. Below is the proposed inventory. Each item names the business operation, the backend responsibility, and the mapping from current-world Lua FCALLs.

The inventory assumes one trait (not multiple domain-split traits) per §6.2; a consumer holding a `Backend: EngineBackend` gets the full write surface without composing four traits.

#### §3.1.1 Claim + lifecycle ops

**1. `claim(lane, capabilities, policy) -> Option<ClaimHandle>`.** Combines today's scheduler grant issuance (`ff_issue_claim_grant`) and the execution claim proper (`ff_claim_execution`). Internally, the Valkey impl runs both FCALLs — possibly in a pipeline if the scheduler has already pre-staged a grant — and returns an opaque handle. The handle carries whatever state the backend needs to subsequently operate on the claim (lease token, exec id, attempt id, capability binding); to the caller it is a typed cookie. Postgres impl: single `SELECT ... FOR UPDATE SKIP LOCKED` + `INSERT INTO leases` in one transaction.

*Rationale for combining grant + claim:* from the worker's standpoint "I got some work" is one event. The scheduler-vs-worker split in today's code is an internal routing optimisation (grants live in a separate partition so scheduling iteration is bounded); it is not a user-visible state transition. Backends that don't split routing (Postgres has no shard concept at this level) collapse the two. Split trait methods would leak the Valkey-specific pre-staging model into the contract. See §9.1 for the debate on whether to instead split the two.

*Maps to Valkey FCALLs:* `ff_issue_claim_grant` (if not pre-staged) + `ff_claim_execution` or `ff_claim_resumed_execution`. The backend picks which claim variant based on the grant type carried internally.

**2. `renew(handle) -> Result<LeaseRenewal, BackendError>`.** Lease renewal. `LeaseRenewal` carries the new expires-at timestamp (monotonic on Valkey via `now_ms`; coordinator clock on Postgres). If the lease has been stolen — fence-triple mismatch — the backend returns a typed error; the caller terminates the attempt. The handle is *not* consumed; renewal is a read-like mutation.

*Maps to:* `ff_renew_lease`.

**3. `progress(handle, update) -> Result<(), BackendError>`.** Progress / heartbeat with an optional typed update. `ProgressUpdate` has variants for percentage + message, frame append, or both — the caller specifies what they're reporting. Backends may coalesce internally; callers see a single op.

*Maps to:* `ff_update_progress` and/or `ff_append_frame`. The Valkey impl picks per variant. The previous two-SDK-method split (`update_progress` + `append_frame`) collapses because the distinction is implementation-internal — both mutate exec_core progress state.

*Sub-note (addressed in §9.4):* this is the one deliberate merge in the inventory. Frame append is load-bearing for stream-based status APIs, and hiding it behind `progress` rather than exposing it as a top-level method is a judgement call that the debate rounds should challenge.

**4. `complete(handle, payload) -> Result<(), BackendError>`.** Terminal success. Consumes the handle. Payload is the attempt's result bytes (caller-chosen encoding; trait doesn't mandate JSON). On Valkey, complete runs a single 12-KEY FCALL that mutates exec state, flow-membership terminal set, unblocks children, and publishes a completion event. On Postgres, a single transaction mutates `executions`, `flow_memberships`, `child_dependencies`, and inserts into `completion_events`.

*Maps to:* `ff_complete_execution`.

**5. `fail(handle, reason, classification) -> Result<FailOutcome, BackendError>`.** Terminal failure (possibly with retry scheduling). `FailOutcome` carries whether the failure triggered a retry (with the new attempt's delay) or was final. The retry-decision policy is backend-internal — retry table, budget admission, and delay calculation all live inside the FCALL on Valkey and the transaction on Postgres. Caller sees the outcome.

*Maps to:* `ff_fail_execution`. Includes the retry-scheduling sub-branches today visible as Lua status codes.

**6. `cancel(handle, reason) -> Result<(), BackendError>`.** Terminal cancellation. Consumes the handle. Reason is an opaque caller string for observability; trait does not constrain its vocabulary.

*Maps to:* `ff_cancel_execution`.

**7. `suspend(handle, waitpoints, timeout) -> Result<SuspendToken, BackendError>`.** Lease-releasing suspension. The attempt hands the lease back and waits for any of a set of waitpoints to fire, or for the timeout. `SuspendToken` is distinct from `Handle` — it is the reclaim credential for resuming the specific suspension. Waitpoints are a typed `Vec<WaitpointSpec>` carrying HMAC-signed tokens generated by the backend. Timeout is optional.

*Maps to:* `ff_suspend_execution` + `ff_create_pending_waitpoint`.

**8. `resume(signals) -> Option<ResumeHandle>`.** Observes signal fires and hands back a new claim-equivalent handle. `ResumeHandle` is structurally identical to `ClaimHandle` from the worker's perspective (both represent "I hold a fenced lease on an attempt") but is typed distinctly to prevent accidentally resuming a non-suspended claim. Returns `None` if no signal has fired yet (non-blocking); blocking is a convenience layer on top.

*Maps to:* `ff_claim_resumed_execution`. The signal-observation side is currently `resume_signals` in `ClaimedTask`; under the new trait that is a sub-detail of `resume`.

#### §3.1.2 Read-path ops (leveraging Phase 1)

**9. `describe_execution(id) -> Option<ExecutionSnapshot>`.** Uses `ff_core::contracts::ExecutionSnapshot` directly; no reshaping. Already shipped by Phase 1 as an inherent method; promoted to trait here to round out the read surface.

*Maps to:* the read path sealed by issue #58.1.

**10. `describe_flow(id) -> Option<FlowSnapshot>`.** Symmetric with 9.

*Maps to:* issue #58.2.

**11. `cancel_flow(id, policy, wait) -> CancelFlowResult`.** Flow-level cancel. `policy` enumerates cascade semantics (cancel-all, cancel-pending-only, etc.; already defined in `ff_core::contracts`). `wait` is whether to block for termination. Returns the set of (id, outcome) pairs — already shipped by RFC-011-adjacent work.

*Maps to:* `ff_cancel_flow` + the server-side orchestration around it.

#### §3.1.3 Out-of-band ops

**12. `report_usage(handle, budget, dimensions) -> Result<AdmissionDecision, BackendError>`.** Budget reporting with optional admission gating. Today, budget reporting is physically embedded on the hot path of `complete`/`fail` via the `ff_report_usage_and_check` Lua wrapper and a report-check ARGV bundle. The trait splits them: `report_usage` runs in its own op-slot so the complete-path doesn't grow with budget feature scope.

*Rationale for splitting out:* budget reporting has its own failure modes (admission failure, quota exhaustion) that today require threading through the complete/fail paths' result types. By giving it its own method, `complete`'s signature stays clean and `fail`'s stays focused on retry scheduling. The hot-path cost is one additional FCALL per attempt for budget-using workloads; that overhead is already paid today (the report-check path makes the same FCALL), just hidden inside the complete wrapper.

*Maps to:* `ff_report_usage_and_check`.

### §3.2 Explicitly not in the trait (with justifications)

Four categories of write-like operation are omitted from `EngineBackend` deliberately. Each justification names the alternative surface.

**Tag reads and writes.** Today `ExecutionSnapshot.tags` carries the tag map. Writes to tags happen at create-time (on `ff_create_execution`) and nowhere else in the SDK-level API. Additional tag-mutation ops (if ever wanted) fold into `describe_execution` for reads and a small `set_tags` op on a separate `AdminBackend` trait. Not on the core path; not on this trait.

**Waitpoint HMAC rotation.** Admin-only operation, run by operators during key rotation, not by worker code. Lives on a parallel `AdminBackend` trait (`rotate_waitpoint_hmac`, `issue_hmac_rotation_grace_period`). A worker with an `EngineBackend` handle has no business rotating keys. Separating admin concerns keeps the hot-path trait lean.

**Stream reads (XRANGE / XREAD).** Stream access is a related-but-distinct decoupling issue (#92). Stream cursors are a different abstraction (they need a cursor type; XRANGE markers don't map to row-based backends without a translation layer). A separate `StreamBackend` trait owns the stream surface. Callers who want both hold two trait objects or a composite. This RFC names StreamBackend's existence; its shape lives in the #92 follow-up RFC.

**Completion pubsub subscription.** Issue #90 files a `CompletionStream` trait. This RFC folds the trait's return-types into `EngineBackend`'s shape — specifically, any trait method that today returns a "subscribe to completions for this entity" side-effect returns a `CompletionStream` typed by the associated type (§4). The stream type itself is this RFC's responsibility (it shapes return types); the subscription mechanism for bulk-tailing the completion channel is #90's trait.

### §3.3 Trait method signatures (sketch)

Each method has a Rust signature sketch below. Types use `ff_core::contracts` vocabulary where available. The sketch is illustrative; final signatures land on the implementation PRs per-method.

> **RFC-only note:** the signatures are Rust-prose for debate purposes. They are not compiled. The debate rounds should treat them as semantic proposals, not compilable interfaces.

```text
trait EngineBackend: Send + Sync + 'static {
    type Handle: Send + Sync;
    type ResumeHandle: Send + Sync;
    type SuspendToken: Send + Sync;
    type Error: std::error::Error + Send + Sync + 'static;
    type CompletionStream: Stream<Item = CompletionEvent> + Send + Unpin;

    async fn claim(
        &self,
        lane: &LaneId,
        capabilities: &CapabilitySet,
        policy: ClaimPolicy,
    ) -> Result<Option<Self::Handle>, Self::Error>;

    async fn renew(
        &self,
        handle: &Self::Handle,
    ) -> Result<LeaseRenewal, Self::Error>;

    async fn progress(
        &self,
        handle: &Self::Handle,
        update: ProgressUpdate,
    ) -> Result<(), Self::Error>;

    async fn complete(
        &self,
        handle: Self::Handle,
        payload: Option<Bytes>,
    ) -> Result<(), Self::Error>;

    async fn fail(
        &self,
        handle: Self::Handle,
        reason: FailureReason,
        classification: FailureClass,
    ) -> Result<FailOutcome, Self::Error>;

    async fn cancel(
        &self,
        handle: Self::Handle,
        reason: &str,
    ) -> Result<(), Self::Error>;

    async fn suspend(
        &self,
        handle: Self::Handle,
        waitpoints: Vec<WaitpointSpec>,
        timeout: Option<Duration>,
    ) -> Result<Self::SuspendToken, Self::Error>;

    async fn resume(
        &self,
        token: Self::SuspendToken,
    ) -> Result<Option<Self::ResumeHandle>, Self::Error>;

    async fn describe_execution(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<ExecutionSnapshot>, Self::Error>;

    async fn describe_flow(
        &self,
        id: &FlowId,
    ) -> Result<Option<FlowSnapshot>, Self::Error>;

    async fn cancel_flow(
        &self,
        id: &FlowId,
        policy: CancelFlowPolicy,
        wait: CancelFlowWait,
    ) -> Result<CancelFlowResult, Self::Error>;

    async fn report_usage(
        &self,
        handle: &Self::Handle,
        budget: &BudgetId,
        dimensions: UsageDimensions,
    ) -> Result<AdmissionDecision, Self::Error>;
}
```

`ResumeHandle` is distinct from `Handle` to prevent passing a freshly-resumed handle to an op that expects a freshly-claimed one (or vice versa). Both types fulfil the same operational role (`&H: Send + Sync` — the handle the worker holds while executing), but at the type level the distinction prevents the class of bug where a consumer threads a resumed handle through a path that expected a claimed-only handle. A single `Handle` associated type is a possible simplification (§9.3); the dual-type design is proposed here and the debate rounds can argue it down if reviewers find it over-specified.

### §3.4 Safety properties preserved

Every trait method must preserve the same fence / idempotency / atomicity properties the current inherent implementation enforces. The trait contract names them; the Valkey backend's current behaviour fulfils them by construction (the Lua functions are already written that way); a future Postgres backend must also fulfil them.

**Fence triple**: every op that mutates attempt state requires `(lease_id, lease_epoch, attempt_id)`. The handle carries these internally; the backend enforces them on every op call. On Valkey, fence-triple checks happen inside the Lua function via keys/args. On Postgres, fence-triple checks happen via `WHERE lease_id = $1 AND lease_epoch = $2 AND attempt_id = $3` in the UPDATE clause.

**Idempotent replay**: `complete`, `fail`, `cancel`, `suspend` are idempotent under replay — calling twice with the same handle and args returns the same result (success on first call, "already in terminal state" on subsequent calls). The trait contract mandates this. Callers can safely retry under network uncertainty.

**Atomicity**: per-op state transitions are atomic — partial state is not observable. On Valkey this is the single-FCALL-per-op property (RFC-011 §5.5 closed the last cross-partition gap). On Postgres this is the per-transaction property. The trait contract names the atomicity requirement; a backend that can't honour it (e.g., a NATS-backed design that can't promise cross-subject atomicity) either must not implement `EngineBackend` or must degrade specific ops with a documented contract deviation.

**Monotonic lease epoch**: lease epoch only advances. No op moves it backward. The backend enforces; the trait contract names the property for consumer-side correctness proofs.

---

## §4 Associated types

Four associated types. Each named below with its role, Valkey impl shape, Postgres impl sketch, and debate-round provenance.

### §4.1 `type Handle: Send + Sync`

Opaque claim credential. Owned by the worker for the duration of the attempt. Consumed by terminal ops (`complete`, `fail`, `cancel`, `suspend`); borrowed by non-terminal ops (`renew`, `progress`, `report_usage`).

**Valkey impl:** the current `ClaimedTask` internals — exec id, attempt id, lease id, lease epoch, capability binding, execution-partition handle, lane id, tag map, input-payload reference. Approximately the current `ClaimedTask` struct minus the caller-facing accessor methods (those move to trait sub-methods or go away).

**Postgres impl:** a struct carrying the row's primary key + lease token (opaque bytes) + fence-triple. No partition concept.

**Why opaque:** the handle's internals are backend-specific. Exposing them on the trait forces consumers to know about Valkey partitions or Postgres row keys. Keeping it opaque means a multi-backend SDK user can write `backend.complete(handle, payload)` once, regardless of backend.

### §4.2 `type Error: std::error::Error + Send + Sync + 'static`

Backend-agnostic error. Extends `EngineError` (Phase 1, issue #58.6) with the small number of additional variants any backend needs — primarily `Self::Error: From<EngineError>` for the error classes already defined, plus backend-specific `Transport` and `Unavailable` variants the backend owns.

**Valkey impl:** `ValkeyBackendError` enum wrapping `ScriptError`, `ferriskey::Error`, and `EngineError` per the Phase 1 classification. The existing `SdkError` splits: its `Validation` / `Contention` / `Conflict` / `State` / `Bug` variants move to `EngineError` (already there per #58); its `Valkey(ferriskey::Error)` variant becomes `ValkeyBackendError::Transport(ferriskey::Error)` on the Valkey-impl-only side. The trait contract exposes only the `EngineError`-shaped variants; the Valkey-specific transport variants never surface.

**Postgres impl:** `PostgresBackendError` wrapping `tokio_postgres::Error` or `sqlx::Error` depending on driver choice (backend-author choice, not trait-level concern).

**The key invariant:** for any `B: EngineBackend`, `B::Error` must be classify-able into the same `EngineError` buckets the Phase 1 work defined. A consumer catching errors doesn't care whether the underlying failure was Valkey `CROSSSLOT` or Postgres `serialisation_failure`; they care whether it's `Contention::RetryRecommended` or `State::AlreadyTerminal`. The trait does not enforce this at the type level (Rust lacks a "my error must project into X" constraint); it is a contract the RFC names explicitly.

### §4.3 `type ResumeHandle: Send + Sync` and `type SuspendToken: Send + Sync`

Distinct from `Handle`. See §3.3 rationale. Either can be collapsed into `Handle` with a `HandleKind` discriminator, and §9.3 argues both positions. The dual-type proposal preserves type-level safety at the cost of more generics to thread through SDK wrappers.

### §4.4 `type CompletionStream: Stream<Item = CompletionEvent> + Send + Unpin`

The pubsub/LISTEN-NOTIFY abstraction. Folded into this RFC because it shapes return types of any op that semantically completes with a "subscribe me to downstream events" side-effect. Today's example: `FlowFabricWorker::observe_completions` (there isn't one at the SDK level; cairn-fabric builds its own via raw ferriskey pubsub). Under the trait, `observe_completions(filter) -> CompletionStream` is a natural method on a related trait (issue #90's CompletionBackend), and that trait's Stream type is this associated type reused.

**Valkey impl:** a wrapper around a ferriskey pubsub subscription on `ff:dag:completions`, filtered client-side (per-subscription) down to the consumer's edge-set.

**Postgres impl:** a LISTEN/NOTIFY subscription filtered server-side via `NOTIFY` payload matching, or a logical-replication tail for higher-volume use cases.

**CompletionEvent type:** `ff_core::contracts::CompletionEvent` — not defined as of writing; ships as part of #90's scope. This RFC authorises its existence; the type shape is #90's deliverable.

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

**Scope.** A new crate `ff-backend-postgres` lands with a minimal `EngineBackend` impl — some methods work, others `unimplemented!()` with a panic message pointing at the tracking issue. The minimum-surface is claim / renew / progress / complete / fail / describe_execution. suspend/resume, cancel_flow, report_usage, etc. remain `unimplemented!()` until demand materialises.

**This is not a shippable Postgres backend.** It is a proof-of-concept that the trait shape is implementable against a non-Valkey backend. Its existence validates that the Stage 1 trait design did not accidentally bake in Valkey-specific assumptions; if it did, Stage 1 reopens.

**Landing gate.** Integration test suite against a Postgres 15 container. Limited to the implemented method set.

**Non-ship contract.** The crate ships under `[features]` or with a `README` that says "experimental; non-production." No version promise.

### §5.4 Stage 4 — Cairn-fabric migration (cairn-team timeline)

Cairn migrates at their own pace. Our side: keep the `EngineBackend` trait stable (no breaking changes post-Stage 1 without a minor version bump), provide the `ff-backend-valkey` crate, provide the migration guide. Cairn's team decides when to adopt.

No gate on our side. This stage completes when cairn files a PR closing their migration.

### §5.5 Stage 5 — Seal `ff_core::keys` as `pub(crate)`

**Scope.** Once no consumer imports from `ff_core::keys` (the raw Valkey key-name builders — `ExecKeyContext`, `IndexKeys`, etc.), flip the module visibility to `pub(crate)` inside the `ff-backend-valkey` crate. External consumers are now fully abstracted from Valkey key names.

**Landing gate.** A `cargo check` against a list of all pinned consumers (just cairn-fabric as of writing) with the sealed module must pass. Confirms no leak remaining.

**Rollback.** Revert the visibility flip if a consumer hits an uncovered use case — the trait needs another method, not a leak of keys. Exception path: a consumer that builds its own diagnostic tooling on raw keys. If that emerges, we expose a `DiagnosticsBackend` trait or a typed key-view API; we do not re-open `pub keys`.

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

**Noted for debate, not rejected outright.** A trait that is always consumed as `Arc<dyn EngineBackend>` simplifies signatures but pays the dynamic dispatch cost per op. Given hot-path ops are dominated by I/O (FCALL round-trip is microseconds; the vtable is nanoseconds), the cost is negligible. The proposal here is that the trait is usable both ways — `<B: EngineBackend>` for static-dispatch consumers and `Arc<dyn EngineBackend>` for dynamic-dispatch consumers — but this forces the `Handle`, `Error`, etc. associated types to be object-safe. §9.5 debates whether to pin to one dispatch style.

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

### §7.2 Capability routing — bitfield or stringly-typed?

**The choice.** `CapabilitySet` — the type threading through `claim`'s signature. Today capabilities are `Vec<String>` on `WorkerConfig` (per `crates/ff-sdk/src/config.rs`) and string-matched at the Lua level. Bitfields would be tighter; stringly-typed is more flexible.

**Lean:** stringly-typed (matches today). Debate: whether `CapabilitySet` should be a newtype over `Vec<String>` or an `HList`-style typed representation.

### §7.3 Does `suspend` return a fresh `Handle` on resume, or does `Handle` itself carry suspend state?

**The choice.** Three shapes:
- **(a)** `Handle` + `ResumeHandle` distinct (as proposed in §3.3). `suspend` consumes `Handle` and returns `SuspendToken`; `resume(SuspendToken)` returns `Option<ResumeHandle>`; subsequent ops consume/borrow `ResumeHandle`. Max type safety.
- **(b)** Single `Handle` type with an internal `HandleKind` discriminant. `suspend` consumes and returns a new handle in "Suspended" kind; `resume` transitions it to "Active" kind. Simpler generics; runtime-checked handle-state invariants.
- **(c)** Single `Handle` and no return from `suspend` — handle is externally consumed; signal-observation returns a fresh handle independently. Cleanest signature; loses the "I know my suspend went through" confirmation.

**Lean:** (a), with (b) as fallback if the generics ergonomics become unworkable in practice.

### §7.4 Do we need a `Batch` op for multi-execution submission?

**The choice.** The current SDK has no batch-submit — each `create_execution` is one call. A future high-throughput consumer might want `submit_batch(Vec<ExecutionRequest>) -> Vec<Result<ExecutionId, _>>`. Adding it to the trait now is speculative; adding it later is a minor version bump if additive-default-impl suffices.

**Lean:** defer. Add when a consumer asks.

### §7.5 `progress` merge — is frame-append a top-level op?

**The choice.** §3.1.1 merged `update_progress` + `append_frame` into one `progress(update: ProgressUpdate)` method. The downside: a consumer that only appends frames (never percent-progress) has to spell `progress(handle, ProgressUpdate::Frame(f))` for what was `append_frame(f)`.

**Lean:** merge. The merge collapses two variants of the same underlying mutation; the call-site verbosity is minor.

### §7.6 `report_usage` split from `complete` — which crate owns it?

**The choice.** Today, `report_usage_and_check` is callable mid-attempt *and* is wrapped into the complete path. The trait splits it: mid-attempt calls go through `report_usage`, terminal ones go through `complete` (which internally may call the backend's usage-settle path). This means there are two slightly-different accounting moments. Is that a consumer confusion point?

**Lean:** clear in practice — "report_usage is mid-attempt only; complete settles terminal accounting." Document in trait rustdoc.

### §7.7 Object-safety constraint: force or leave flexible?

**The choice.** §6.4. Object-safety costs us a bit of associated-type flexibility (can't return `impl Stream` directly; must use `Pin<Box<dyn Stream>>`). Static-dispatch callers don't care; dynamic-dispatch callers benefit.

**Lean:** force object-safety. The consumer population includes embedding-mode (generics fine) and dynamic mode (trait objects needed); forcing object-safety serves both; the cost is one indirection for static-dispatch callers.

### §7.8 Trait location — `ff-core`, `ff-sdk`, or a new `ff-backend-api` crate?

**The choice.** The trait definition must live somewhere all backend crates + the SDK can depend on. Candidates:
- `ff-core` — already the shared base. But `ff-core` today has no `async` trait dep; adding one pulls tokio into `ff-core`'s build graph.
- `ff-sdk` — but the Valkey backend sits below the SDK in the dep order.
- New `ff-backend-api` crate — clean separation, one more crate to maintain.

**Lean:** new `ff-backend-api` crate. The dep-graph hygiene justifies the extra crate.

### §7.9 Error-projection constraint — enforceable?

**The choice.** §4.2 asserts that every `B::Error` must be classify-able into `EngineError` buckets. Rust has no language-level way to require this. We could:
- Add a `fn classify(&self) -> EngineErrorClass` method on a sub-trait (`trait BackendError { fn classify() }`) that `Self::Error: BackendError` requires. Enforceable at the type level; adds constraint.
- Leave it as a prose contract. Unenforceable; ergonomic.

**Lean:** enforce via the sub-trait. The cost is ~10 lines; the benefit is catching a misclassifying impl at compile time.

### §7.10 When does `ValkeyBackend::Error` stop being `ferriskey::Error`-shaped?

**The choice.** Stage 1 (§5.1) defines `ValkeyBackendError` wrapping `ferriskey::Error`. Issue #88 asks us to seal `ferriskey::Error` from the public surface. The sealing lands when the trait's `B::Error` is the only error type anyone sees — `ValkeyBackendError`'s `Transport(ferriskey::Error)` variant becomes `pub(crate)` and the public-projection-through-classify only exposes `EngineError` buckets.

**Lean:** seal in Stage 1. The internal variant stays; the pub surface is classify-only.

### §7.11 Feature-flag the trait?

**The choice.** During experimentation, put `pub trait EngineBackend` behind `#[cfg(feature = "backend-trait")]` so consumers don't see it until it's stable. Pre-landing safety net.

**Lean:** no feature flag. This is not an experimental feature; it's the architectural keystone. Feature-flagging implies optional; this is not optional post-Stage 1.

### §7.12 How does `AdminBackend` compose with `EngineBackend`?

**The choice.** A backend author writes `impl EngineBackend for ValkeyBackend` and `impl AdminBackend for ValkeyBackend`. A consumer who wants both holds `Arc<ValkeyBackend>` and uses whichever methods they need. For a user who wants a dyn-dispatch multi-trait setup, `trait Backend: EngineBackend + AdminBackend` can be defined in the consumer's crate.

**Lean:** no Backend super-trait in our code; let consumers compose as they wish.

### §7.13 Backend-specific extension points

**The choice.** A backend might want to expose a method that only makes sense on itself (e.g., `ValkeyBackend::run_raw_fcall` for diagnostic tooling). Where does that go?

**Lean:** inherent methods on the concrete `ValkeyBackend` type, accessible via downcast. Not on any trait. Tooling that needs Valkey-specific introspection explicitly pins to `ValkeyBackend`; other consumers are abstraction-pure.

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

**Response:** §7.3. The type distinction prevents a class of bug (thread a resumed handle through a path that expects a claimed handle). The cost is two extra generics on SDK wrapper signatures. If, during Stage 1 impl, the cost shows up as punitive in practice, collapsing to a single `Handle` with `HandleKind` is an additive change (compat-preserving) — at which point we ship the simplification as a 0.4.1 minor. Forcing the distinction now is the conservative choice; relaxing later is cheap.

### §8.5 "`CompletionStream` as an associated type on EngineBackend bloats the trait."

**Response:** §4.4. Only the *type* lives on `EngineBackend` — the actual subscription methods (`subscribe_completions`, `tail_stream`) live on `CompletionBackend` per issue #90 and `StreamBackend` per issue #92. `EngineBackend` names `CompletionStream` as its associated type because several `EngineBackend` methods return a stream-typed side effect. A trait that returns `impl Stream<...>` in multiple methods needs one associated type declaration for the Stream shape; declaring it once on the base trait is cheaper than declaring it in each returning method's signature.

### §8.6 "Trait-level error classification enforcement (§7.9) is architectural overreach."

**Response:** CONCEDE the concern; the choice is between compile-time enforcement (add 10 LoC of constraint, catch misclassifying impls at `impl EngineBackend for X`) and prose-contract enforcement (zero code cost, bugs caught at review). I believe the 10 LoC is worth it; reviewers who disagree should argue the case in §7.9's debate slot. The RFC is open on this point; I lean "enforce"; I am not hardening that lean before the debate round.

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
