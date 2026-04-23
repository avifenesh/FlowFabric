# RFC-013: Stage 1d — `EngineBackend::suspend` trait migration + input/output-shape rework

**Status:** Draft
**Author:** FlowFabric — Stage 1d
**Created:** 2026-04-23
**Tracking:** #117 (Stage 1d deferral)
**Depends on:** RFC-004 (Suspension), RFC-005 (Signal), RFC-012 (EngineBackend trait, round-7 amendment §R7.6.1)
**Forward-compat with:** RFC-014 (multi-signal resume conditions, parallel — draft)
**Non-goal:** re-designing RFC-004's wire semantics, the HMAC token protocol, or the Lua `ff_suspend_execution` function body.

---

## Summary

`EngineBackend::suspend` is the only round-4 trait method still stubbed at `EngineError::Unavailable`. The Valkey backend impl at `crates/ff-backend-valkey/src/lib.rs:~1240-1249` cannot be filled because (a) the Round-4 trait return type `Result<Handle, EngineError>` cannot express the `AlreadySatisfied` branch in which the caller keeps the lease, and (b) the trait input `Vec<WaitpointSpec>` + `Option<Duration>` has no slot for reason code, requested-by, continuation pointer, resume-condition shape, or timeout behavior — all of which the SDK today packs into ARGV-JSON inside `ClaimedTask::suspend` and which `parse_suspend_result` reads back from raw wire bytes. This RFC fixes both ends: a typed `SuspendArgs` input, a typed `SuspendOutcome` output, and a first-class `ResumeCondition` that RFC-014 can extend without breaking. After landing, `ClaimedTask::suspend` becomes a thin forwarder and `parse_suspend_result` is deleted.

## Motivation

**What breaks today.**

1. `EngineBackend::suspend` returns `EngineError::Unavailable`. Any consumer dispatching through `dyn EngineBackend` loses the suspend operation entirely; only `ClaimedTask::suspend`'s direct FCALL path works. This contradicts RFC-012's stated gate — "every user-facing op forwards through the trait".
2. `ff-sdk::parse_suspend_result` (`task.rs:1448`) reconstructs a `SuspendOutcome` by parsing a positional Lua array. The parser is internal to `ff-sdk`; a hypothetical second backend (Postgres) cannot produce the same array shape without faking `ff_suspend_execution`'s wire contract verbatim. Typed args + typed outcome move the contract into `ff-core` where both backends honour the same Rust types.
3. Round-7 §R7.6.1 pins the **open question** "typed `ResumeCondition` on trait vs ARGV-JSON" for Stage 1d — this RFC closes it.
4. RFC-014 will add multi-signal resume conditions (`all_of`, `count(n)`, composition). Without the input-shape rework landing first, RFC-014 has to break the trait a second time.

**Why v0.6 (the release that ships this):**
- 0.4.x locked in round-7 deferrals with a `report_usage` break.
- 0.5.0 was kept for Stage 1c shipables without trait-shape churn.
- 0.6.0 is the natural window for the last Round-4 breaker. After 0.6.0, RFC-012's Stage-1 envelope is closed.

Driven use cases (from RFC-004): UC-19 (signal wait), UC-20 (external callback), UC-21 (human approval), UC-22 (multi-condition — forward-compat with RFC-014).

## Assumptions

A1. **Round-4 Handle is concrete, not an associated type.** The `Handle` struct is `ff-core::backend::Handle` with `kind: HandleKind`. This RFC does NOT re-open M-D2 (borrow vs. consume). `suspend` borrows the handle; it does not consume it. Terminal-looking behaviour is expressed through the returned `SuspendOutcome` variant (the caller's pre-suspend handle is logically invalidated on `Suspended` and logically retained on `AlreadySatisfied`, but enforcement is runtime, not compile-time — same model as `complete`/`fail`).

A2. **HMAC token protocol from RFC-004 is unchanged.** Kid-ring, 2-kid rotation, `ff:sec:{p:N}:waitpoint_hmac`, grace window — all as shipped. This RFC carries tokens through typed fields; it does not re-negotiate the wire.

A3. **Single active suspension per execution remains a v1 invariant** (RFC-004 §V1 Scope). This RFC does NOT add multi-suspension-per-execution; it reserves the typed shape so RFC-014 can.

A4. **`create_waitpoint` (round-7) stays unchanged.** Pending waitpoints are minted separately; `suspend` may consume a pending waitpoint or create a fresh one, exactly as today's Lua supports via the `use_pending_waitpoint` ARGV flag. This RFC surfaces that flag as a typed `SuspendArgs::pending_waitpoint: Option<WaitpointId>`.

A5. **`ff-server` REST consumers do NOT call `suspend` directly.** Suspension is worker-originated; the only caller is `ClaimedTask::suspend` inside `ff-sdk`. This keeps the blast radius contained: one forwarder rewrite, one parser deletion, zero public REST break.

---

## §1 — What breaks today (concrete)

- `crates/ff-backend-valkey/src/lib.rs:~1240-1249` — stub returning `EngineError::Unavailable { op: "suspend" }`.
- `crates/ff-sdk/src/task.rs:812-924` — `ClaimedTask::suspend` builds 17 KEYS + 17 ARGV by hand, serializes `resume_condition_json` and `resume_policy_json` as ad-hoc JSON, calls `self.client.fcall("ff_suspend_execution", …)` directly, then hands raw wire bytes to `parse_suspend_result`.
- `crates/ff-sdk/src/task.rs:1448-1547` — `parse_suspend_result` duplicates the Lua return contract in Rust. Any schema drift in `ff_suspend_execution` silently de-syncs from this parser.
- `ff-core::backend::WaitpointSpec` (landed round-7 for `create_waitpoint`) has `kind: WaitpointKind`, `matcher: Vec<u8>`, `hmac_token: WaitpointHmac`. The `matcher` is an opaque byte blob — a future-proofing hedge, not a typed condition. Stage 1d fills this in.
- `ff-sdk::task::ConditionMatcher` — SDK-local, not on the trait; a `struct { signal_name: String }`. Only expresses "match this signal name" or "wildcard". No count, no all-of, no payload predicate, no timeout-signal synthesis.

The round-7 amendment explicitly deferred this rework (§R7.6.1): "typed `ResumeCondition` on trait in round-8, or ARGV-JSON in backend impl?". This RFC answers: **typed, on the trait**. Rationale in §8.2.

---

## §2 — Proposed trait signature + type shapes

### §2.1 Trait method

```rust
// crates/ff-core/src/engine_backend.rs
async fn suspend(
    &self,
    handle: &Handle,
    args: SuspendArgs,
) -> Result<SuspendOutcome, EngineError>;
```

Rationale for `SuspendArgs` struct over positional params:
- Matches the round-7 precedent: `DeliverSignalArgs`, `ClaimResumedExecutionArgs`, `AppendFrameArgs` all pack op inputs into one `#[non_exhaustive]` struct. Suspending follows the same convention.
- Non-exhaustive gives us RFC-014 additive room without a trait break.
- Named fields beat positional triples for a call site with 6+ parameters.

### §2.2 `SuspendArgs` shape

```rust
// crates/ff-core/src/contracts/mod.rs
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SuspendArgs {
    /// Worker-provided new suspension id (UUID v4). Minted caller-side
    /// so the echoed-back value in `SuspendOutcome` is round-trippable
    /// and the Lua function can run under Valkey's no-random-in-scripts
    /// rule.
    pub suspension_id: SuspensionId,

    /// The waitpoint being activated. Two cases:
    ///   * `Fresh { waitpoint_id, waitpoint_key }` — mint a fresh active
    ///     waitpoint as part of `suspend`. `waitpoint_id` and `waitpoint_key`
    ///     are worker-minted (UUID, `wpk:<uuid>`).
    ///   * `UsePending { waitpoint_id }` — activate a pending waitpoint
    ///     previously issued by `create_waitpoint`. The key + HMAC token
    ///     were returned from that call; `suspend` resolves them from the
    ///     waitpoint hash.
    pub waitpoint: WaitpointBinding,

    /// Declarative resume condition. Typed. See §2.4.
    pub resume_condition: ResumeCondition,

    /// Resume-side policy (what happens when the condition is satisfied).
    /// Typed. See §2.5.
    pub resume_policy: ResumePolicy,

    /// Reason category — maps to RFC-004's `reason_code` (one of the
    /// suspension reason enum values); carried over the trait so the
    /// backend does not need to re-infer it from `resume_condition`.
    pub reason_code: SuspensionReasonCode,

    /// Who requested suspension — `Worker`, `Operator`, `Policy`,
    /// `SystemTimeoutPolicy`.
    pub requested_by: SuspensionRequester,

    /// Wall-clock deadline. `None` = suspend indefinitely until a
    /// signal/operator action satisfies the condition.
    ///
    /// Validated against **backend wall-clock** (Valkey `TIME`), not
    /// the caller's `now`. Worker clock skew up to
    /// `SUSPEND_CLOCK_SKEW_TOLERANCE_MS` (600000 ms = 10 min, per the
    /// HMAC-grace-window precedent in RFC-004) is accepted; beyond
    /// that, validation rejects as `timeout_at_in_past`. Callers may
    /// query backend time via `ff_sdk::diagnostics::server_time()` if
    /// they need to compute a skew-safe deadline.
    pub timeout_at: Option<TimestampMs>,

    /// What happens at `timeout_at` if the condition is unsatisfied.
    /// Typed enum (RFC-004 §Timeout Behavior); moved off the SDK's
    /// ad-hoc `TimeoutBehavior` str.
    pub timeout_behavior: TimeoutBehavior,

    /// Opaque worker-owned continuation pointer. The engine stores it
    /// and hands it back on resume; it is NOT interpreted by the
    /// engine (RFC-004 §Continuation Model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_metadata_pointer: Option<String>,

    /// `now` at caller time — the backend uses server-time `TIME` for
    /// Lua correctness but carries the caller clock for correlation.
    pub now: TimestampMs,

    /// Optional idempotency key for retry-safe suspension. When set, the
    /// backend dedups on `(partition, execution_id, idempotency_key)` for
    /// a configured window: a second `suspend` with the same triple
    /// returns the same `SuspendOutcome` as the first (idempotent replay).
    /// When `None`, every call is fresh and subject to §3's non-idempotent
    /// contract. Follows the `UsageDimensions.dedup_key` pattern from
    /// RFC-012 §R7.2.3 — same shape, same semantics, same partition-scoped
    /// dedup hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<IdempotencyKey>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub enum WaitpointBinding {
    Fresh {
        waitpoint_id: WaitpointId,
        waitpoint_key: String,
    },
    UsePending {
        waitpoint_id: WaitpointId,
    },
}
```

Notes:

- `suspension_id` / `waitpoint_id` / `waitpoint_key` are worker-minted. This mirrors today's SDK (see `task.rs:834-837`) and matches how Valkey scripts must work (no `math.random` in FCALLs).
- `continuation_metadata_pointer` stays `Option<String>` — same opacity as today. RFC-004 explicitly does not let the engine own continuation bytes.
- `reason_code` + `requested_by` are typed enums (currently ARGV strings). The enum values are the set defined in RFC-004 §Suspension Reason Categories; `#[non_exhaustive]` so RFC-014/RFC-015 can extend.
- `WaitpointSpec` (from round-7) is **not** reused as the `suspend` input: it was shaped for `create_waitpoint`'s pending issuance. Keeping them separate is cleaner than overloading one struct with conditional fields.
- `idempotency_key` is a partition-scoped dedup key. Lua stores a hash `ff:dedup:suspend:{p:N}:<execution_id>:<idempotency_key>` → serialized `SuspendOutcome`, TTL = `min(timeout_at - now + SUSPEND_DEDUP_GRACE_MS, SUSPEND_DEDUP_MAX_TTL_MS)` where `SUSPEND_DEDUP_GRACE_MS = 60_000` (1 min) and `SUSPEND_DEDUP_MAX_TTL_MS = 604_800_000` (7 days). When `timeout_at` is `None`, TTL caps at `SUSPEND_DEDUP_MAX_TTL_MS`. On hit, the first outcome is returned verbatim (including the original `suspension_id` / `waitpoint_id` / `waitpoint_token`), and no state mutation occurs. This mirrors `UsageDimensions.dedup_key`'s semantics on `report_usage`. Absent a key, §3's non-idempotent contract applies.
- **Dedup-hit `suspension_id` semantics.** When a dedup entry is hit, the returned `SuspendOutcome::*.suspension_id` is the **first call's** minted id, NOT the retry's. Callers correlating by `suspension_id` (logs, observability) should use the echoed value from `SuspendOutcome`, not the value they passed in — the retry's `suspension_id` is silently discarded. This is the idempotent-replay contract; the request `suspension_id` is effectively a cache key input on dedup-miss and a no-op on dedup-hit.

### §2.2.1 Constructors (required per `non_exhaustive`)

`SuspendArgs`, `WaitpointBinding`, and `ResumePolicy` are all `#[non_exhaustive]`. The following constructors ship with this RFC:

```rust
impl SuspendArgs {
    /// Build a minimal `SuspendArgs` for a worker-originated suspension.
    /// Defaults: `requested_by = Worker`, `timeout_at = None`,
    /// `timeout_behavior = Fail`, `continuation_metadata_pointer = None`,
    /// `idempotency_key = None`. Override via `with_*` setters.
    pub fn new(
        suspension_id: SuspensionId,
        waitpoint: WaitpointBinding,
        resume_condition: ResumeCondition,
        resume_policy: ResumePolicy,
        reason_code: SuspensionReasonCode,
        now: TimestampMs,
    ) -> Self { /* ... */ }

    pub fn with_timeout(mut self, at: TimestampMs, behavior: TimeoutBehavior) -> Self { /* ... */ }
    pub fn with_requester(mut self, requester: SuspensionRequester) -> Self { /* ... */ }
    pub fn with_continuation_metadata_pointer(mut self, p: String) -> Self { /* ... */ }
    pub fn with_idempotency_key(mut self, key: IdempotencyKey) -> Self { /* ... */ }
}

impl WaitpointBinding {
    /// Mint a fresh `WaitpointBinding::Fresh` with worker-side UUID v4
    /// for `waitpoint_id` and `waitpoint_key = format!("wpk:{id}")`.
    /// Convenience for the common case.
    pub fn fresh() -> Self { /* ... */ }

    /// Construct a `UsePending` binding from a `PendingWaitpoint` handle
    /// returned by `create_waitpoint`.
    pub fn use_pending(pending: &PendingWaitpoint) -> Self { /* ... */ }
}

impl ResumePolicy {
    /// Canonical v1 defaults: `Runnable` target, signals consumed on
    /// satisfy, buffer discarded on close, no resume delay, waitpoint
    /// closed on resume. Use this unless you have a specific reason to
    /// deviate.
    pub fn normal() -> Self {
        Self {
            resume_target: ResumeTarget::Runnable,
            consume_matched_signals: true,
            retain_signal_buffer_until_closed: false,
            resume_delay_ms: None,
            close_waitpoint_on_resume: true,
        }
    }
}
```

Memory-rule compliance: these constructors satisfy `non_exhaustive_needs_constructor` — every public `#[non_exhaustive]` type in this RFC is buildable without reaching inside.

### §2.3 `SuspendOutcome` shape

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SuspendOutcome {
    /// The normal path: execution is now suspended, the worker's
    /// pre-suspend handle is no longer lease-bearing, and `handle`
    /// carries the `HandleKind::Suspended` cookie the caller uses for
    /// `observe_signals` and the eventual `claim_from_reclaim` /
    /// `claim_resumed_execution`.
    Suspended {
        suspension_id: SuspensionId,
        waitpoint_id: WaitpointId,
        waitpoint_key: String,
        waitpoint_token: WaitpointHmac,
        /// Fresh suspended-kind Handle. Replaces the caller's
        /// pre-suspend handle (same concrete struct, same BackendTag,
        /// new opaque bytes).
        handle: Handle,
    },

    /// The early-satisfied path: buffered signals on a pending
    /// waitpoint already matched the resume condition at suspension
    /// time. The lease is NOT released; the caller's pre-suspend
    /// handle remains valid and lease-bearing; the worker continues
    /// executing as if `suspend` never happened, other than the
    /// waitpoint closing as `satisfied`.
    AlreadySatisfied {
        suspension_id: SuspensionId,
        waitpoint_id: WaitpointId,
        waitpoint_key: String,
        waitpoint_token: WaitpointHmac,
        // NO `handle` field: caller keeps their pre-suspend handle.
    },
}
```

**Why an enum of structs (not a single struct with an `Option<Handle>`):**
- The `Suspended` branch always has a fresh Handle; the `AlreadySatisfied` branch never does. Encoding that as `Option` invites null-confusion bugs at the call site. §8.1 argues this more.
- Both variants carry the HMAC `waitpoint_token`. On `AlreadySatisfied` the token exists but the waitpoint is closed; we still return it for uniform parse shape and for operator traceback. Worker code should not re-use it for signal delivery — but that's enforced by the waitpoint's `closed=1` state in Lua, not by omission here.

**Handle-kind semantics** (concrete, not theoretical):
- Pre-suspend: `HandleKind::Fresh` or `HandleKind::Resumed` (either works — both are lease-bearing).
- On `Suspended`: returned `handle.kind == HandleKind::Suspended`. Caller's pre-suspend handle is NOT automatically invalidated by Rust (no move/drop); runtime enforcement: any trait op called against the stale handle returns `EngineError::Contention(LeaseConflict)` because the fence triple (lease_id/epoch/attempt_id) in that handle's opaque bytes no longer matches the now-`unowned` exec_core.
- On `AlreadySatisfied`: no new handle. Caller's pre-suspend handle remains valid (lease still held, fence triple still matches). This is the load-bearing invariant — consumers must pattern-match both variants and only substitute handles on `Suspended`.

### §2.4 `ResumeCondition` shape (forward-compat with RFC-014)

```rust
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResumeCondition {
    /// Single waitpoint-key match with a matcher predicate. Replaces
    /// the SDK's v1 `ConditionMatcher`. `matcher` is the per-waitpoint
    /// predicate (signal-name-or-wildcard in v1; RFC-014 may extend).
    Single {
        waitpoint_key: String,
        matcher: SignalMatcher,
    },

    /// Operator-only resume — no signal ever satisfies; only an
    /// explicit operator resume closes. Used for escalate-style paths.
    OperatorOnly,

    /// Timeout-only — waitpoint has no signal satisfier, only the
    /// `timeout_behavior` at `timeout_at` resolves it. (Useful for
    /// pure-delay suspensions where a signal arriving would be an
    /// error, not a resumer.)
    TimeoutOnly,

    /// Multi-condition composition. The concrete body
    /// (`CompositeBody`) is defined by RFC-014; RFC-013 only reserves
    /// the enum slot so both RFCs can land without a trait break.
    /// RFC-014's body is required to be additively extensible (nested
    /// `ResumeCondition`s, `#[non_exhaustive]` on the body type) so
    /// subsequent RFCs can grow the composition vocabulary without
    /// breaking RFC-013's surface.
    Composite(CompositeBody),
}
```

**Forward-compat hooks (RFC-014 will extend):**
- `#[non_exhaustive]` lets RFC-014 add further top-level variants (e.g. `Count { signal_name, n }`) without breaking downstream pattern matches.
- The `Composite(CompositeBody)` variant is RFC-013's reserved extension point. RFC-014 defines `CompositeBody` — expected shape: `enum CompositeBody { AllOf(Vec<ResumeCondition>), AnyOf(Vec<ResumeCondition>), Count { clause: Box<ResumeCondition>, n: u32 }, … }` with `#[non_exhaustive]`. The flat `AnyOf { signal_names: Vec<String> }` / `AllOf { signal_names: Vec<String> }` shapes an earlier draft proposed are **dropped**: they are a strict subset of RFC-014's recursive composites (e.g. `AnyOf { signal_names: ["a","b"] }` ≡ `Composite(CompositeBody::AnyOf(vec![Single{...a}, Single{...b}]))`). Keeping both would be redundant surface area.
- `minimum_signal_count` (RFC-004 §Resume Condition Fields) is NOT a top-level `SuspendArgs` field; it is implicit per variant — `Composite(AllOf(clauses))` ⇒ `n = |clauses|`; `Composite(AnyOf(clauses))` ⇒ `n = 1`; `Composite(Count{n,…})` (RFC-014) carries an explicit `n`.

**`SignalMatcher` shape:** v1 is `enum SignalMatcher { ByName(String), Wildcard }`. RFC-014 may extend (payload predicates, etc.) — `#[non_exhaustive]`.

**`waitpoint_key` cross-field invariant.** When both `WaitpointBinding::Fresh { waitpoint_key: A }` and `ResumeCondition::Single { waitpoint_key: B, .. }` are present, Rust-side validation enforces `A == B`. Mismatch → `Validation(InvalidInput { detail: "waitpoint_key_mismatch" })`, pre-FCALL. For `WaitpointBinding::UsePending`, the Lua-side lookup supplies the authoritative `waitpoint_key`; if `ResumeCondition::Single.waitpoint_key` is provided and does not match, Lua returns `invalid_waitpoint_for_execution` → `Validation(InvalidWaitpointForExecution)`.

**Why not JSON passthrough:**
- The round-7 §R7.6.1 open question offered "ARGV-JSON in backend impl" as an alternative. Rejected because: (i) a Postgres backend cannot produce the same JSON shape without re-implementing the Valkey Lua's `initialize_condition` helper; (ii) typed variants let `rustc` enforce exhaustiveness on the SDK forwarder — replacing runtime parse errors with compile errors when RFC-014 lands; (iii) JSON-at-the-seam means every consumer of `dyn EngineBackend` has to agree on a schema that is otherwise undocumented.
- Cost: the Valkey backend still serializes `ResumeCondition` → ARGV JSON (matches today's `ff_suspend_execution` contract). That's an internal, backend-scoped serializer, not a public API.

### §2.5 `ResumePolicy` shape

```rust
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResumePolicy {
    /// Where the satisfied suspension goes. V1: only `Runnable`.
    pub resume_target: ResumeTarget,

    /// Whether matched signals are considered consumed after satisfaction.
    pub consume_matched_signals: bool,

    /// Whether buffered signals remain readable after waitpoint close.
    pub retain_signal_buffer_until_closed: bool,

    /// Optional delay before the resumed execution becomes eligible
    /// (RFC-004 §Resume policy fields `resume_delay_ms`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_delay_ms: Option<u64>,

    /// Normally true. Closes the waitpoint as part of resume.
    pub close_waitpoint_on_resume: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResumeTarget {
    /// Execution returns to `runnable` and goes through normal
    /// scheduling. V1 default.
    Runnable,
}
```

Only one `ResumeTarget` variant today — `Runnable` — but `#[non_exhaustive]` on both leaves room for RFC-014's `DirectActive` (inline re-claim) if that becomes desirable.

### §2.6 `TimeoutBehavior`, `SuspensionReasonCode`, `SuspensionRequester`

These are direct translations of RFC-004's string-based fields into typed enums:

```rust
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum TimeoutBehavior {
    Fail,
    Cancel,
    Expire,
    AutoResumeWithTimeoutSignal,
    Escalate, // v2 per RFC-004 Implementation Notes; enum slot present.
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum SuspensionReasonCode {
    WaitingForSignal,
    WaitingForApproval,
    WaitingForCallback,
    WaitingForToolResult,
    WaitingForOperatorReview,
    PausedByPolicy,
    PausedByBudget,
    StepBoundary,
    ManualPause,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum SuspensionRequester {
    Worker,
    Operator,
    Policy,
    SystemTimeoutPolicy,
}
```

All three are `#[non_exhaustive]`; backend serializes to the corresponding Lua/wire string.

---

## §3 — Replay + idempotency contract

### §3.1 Valid replay cases

1. **Same handle, same `SuspendArgs` (including same `suspension_id`), second call after transport error on the first.**
   - Semantics: the first call either committed (exec is now `suspended`) or did not.
   - Behavior:
     - If the first call committed: the fence triple in `handle`'s opaque bytes still names the pre-suspend lease — now invalidated. The second call returns `EngineError::Contention(ContentionKind::LeaseConflict)` (lease_id no longer matches exec_core's `current_lease_id`, which is empty). The caller MUST treat `LeaseConflict` after a known-possibly-successful `suspend` as "suspend probably succeeded; re-read exec state via `describe_execution` to confirm".
     - If the first call did not commit: same args succeed fresh.
   - **Not idempotent via the `suspension_id`**: Lua does not check for pre-existing `suspension_id` before committing. The second attempt's commit fails on the already-suspended check (see RFC-004 Lua pseudocode `EXISTS KEYS.suspension_current → "already_suspended"`). That returns `EngineError::State(StateKind::AlreadySuspended)`.
   - **Contract**: callers must assume `suspend` is **not** idempotent on retry. On any error after `suspend`, the caller MUST `describe_execution` before retrying, and only retry if lifecycle is still `active`.
   - **Describe-retry budget.** `describe_execution` is itself retryable under normal transport-retry policy (RFC-010 §Transport). The `suspend` retry remains blocked until one describe succeeds; callers may backoff-retry the describe independently without opening a `suspend` double-commit window. With `idempotency_key` set, this reconciliation is unnecessary — the caller retries `suspend` directly and the backend dedups.

2. **Same handle, same args, exec is in `suspended` already (discovered via describe).**
   - Caller should `claim_resumed_execution` or wait for signal — not re-`suspend`.

### §3.2 Invalid replay cases

3. **Second `suspend` on the fresh Suspended-kind handle returned by a successful `suspend`.**
   - `handle.kind == Suspended`. Backend pattern-matches: if kind is Suspended and the caller is trying to transition exec from `active` to `suspended`, return `EngineError::State(StateKind::AlreadySuspended)`. This is a programming bug; it is also cheap to detect before any FCALL (handle kind check in Rust).

4. **Second `suspend` with a new `suspension_id` after the first succeeded.**
   - Equivalent to (3) — the exec is already suspended. `EngineError::State(StateKind::AlreadySuspended)` via Lua `already_suspended` error code.

### §3.3 Contract summary

- `suspend` is **not** retry-idempotent **without** an `idempotency_key`. The `suspension_id` is echoed in `SuspendOutcome` for caller correlation, not for Lua-side dedup.
- **With** `SuspendArgs::idempotency_key: Some(_)` (see §2.2), `suspend` is retry-idempotent: the backend keys a dedup hash on `(partition, execution_id, idempotency_key)` with TTL = suspension-timeout ceiling + grace. A second call within the window returns the first call's `SuspendOutcome` verbatim and performs no state mutation. Shape + semantics follow RFC-012 §R7.2.3's `UsageDimensions.dedup_key`. Without the key, callers must describe-and-reconcile on any retry.

---

## §4 — Error taxonomy

All errors surface as `EngineError`. Variant-by-variant, by scenario:

| Scenario | `EngineError` variant | Notes |
|---|---|---|
| Fence triple drifted (lease stolen / expired / revoked between call-site and FCALL) | `Contention(LeaseConflict)` | Retryable after `describe_execution` reconcile. |
| Handle kind is Suspended at entry | `State(AlreadySuspended)` | Rust-side check, no FCALL. Programming bug. |
| Exec lifecycle drifted to terminal before FCALL commits | `Contention(ExecutionNotActive { … })` | Mirrors Lua `execution_not_active`. |
| Exec already has `suspension:current` hash set with no `closed_at` | `State(AlreadySuspended)` | Lua `already_suspended`. |
| Lease id/epoch/attempt mismatch on the handle | `Validation(InvalidLeaseForSuspend)` | Lua `invalid_lease_for_suspend`. |
| `WaitpointBinding::UsePending` but waitpoint is missing / wrong exec / expired | `Contention(WaitpointNotFound)` or `State(PendingWaitpointExpired)` | Pre-existing variants from RFC-004 wire. |
| `WaitpointBinding::Fresh` but `waitpoint_key` does not parse to `wpk:<uuid>` | `Validation(InvalidWaitpointKey)` | Pre-existing. |
| `resume_condition == Single { matcher: ByName("") }` with no wildcard intent, or RFC-014 `Composite` with empty clause list | `Validation(InvalidInput { detail: "resume_condition" })` | Rust-side check before FCALL. |
| `timeout_at` in the past | `Validation(InvalidInput { detail: "timeout_at_in_past" })` | Rust-side. |
| HMAC secrets not initialised for partition | `Transport(boxed ScriptError::hmac_secret_not_initialized)` | Ops issue, not caller issue. |
| Valkey connection error / transient | `Transport(…)` | Standard. |
| Strict `suspend` (non-`try`) but buffered signals already satisfy the condition | `State(AlreadySatisfied)` | SDK-side: strict path refuses to return the early-satisfied branch. The typed backend outcome is `SuspendOutcome::AlreadySatisfied`; the strict wrapper maps it to this error. `try_suspend` returns the outcome directly instead. |
| `ResumeCondition::TimeoutOnly` with `timeout_at.is_none()` | `Validation(InvalidInput { detail: "timeout_only_without_deadline" })` | Rust-side check. Pure-timeout suspend with no deadline would suspend forever with no satisfier. |
| `WaitpointBinding.waitpoint_key` vs `ResumeCondition::Single.waitpoint_key` mismatch (Fresh binding) | `Validation(InvalidInput { detail: "waitpoint_key_mismatch" })` | Rust-side cross-field check, pre-FCALL. For `UsePending`, Lua-side check → `Validation(InvalidWaitpointForExecution)`. |
| `WaitpointBinding::UsePending { waitpoint_id }` but that waitpoint was already activated by a prior committed `suspend` | `State(AlreadySuspended)` | Lua check order: exec-level `already_suspended` fires first (the exec is in `suspended` state), so the waitpoint-level check is never reached. |

**New kinds needed:** one. `StateKind::AlreadySatisfied` is added to surface the strict-path refusal of the early-satisfied branch (the underlying backend outcome is always typed `SuspendOutcome`; only the SDK's strict wrapper turns it into an error). Every other scenario maps to an existing variant after Round-7. The `InvalidLeaseForSuspend` variant was added in Round-4 specifically for this path (confirmed at `crates/ff-core/src/engine_error.rs:183`).

**Enum-extension safety.** `StateKind`, `ValidationKind`, and `ContentionKind` are all `#[non_exhaustive]` (confirmed at `crates/ff-core/src/engine_error.rs:161,227,315`). Adding `StateKind::AlreadySatisfied` is therefore non-breaking for downstream consumers who match exhaustively with a `_ =>` arm — cairn's `EngineError` match ladders survive the upgrade without a trait break.

**`HandleKind::Suspended` pre-check is not dedup-dodgeable.** The Rust-side handle-kind pre-check (first row: `State(AlreadySuspended)`, no FCALL) fires before any `idempotency_key` dedup lookup can reach Lua. A caller cannot "replay" past a Suspended-kind handle by repeating the same idempotency key; that kind signals a programming bug, not a transport retry. Callers who want idempotent-replay safety must pass a fresh handle for the retry — `idempotency_key` only helps when the retry is an honest transport-error retry with an otherwise-valid handle.

**Classification note:** `LeaseConflict` + `ExecutionNotActive` are retryable; `AlreadySuspended` is cooperative (no-op — the caller's intent is already the state); everything else is either terminal (validation, bug) or transport (standard retry). See `EngineError::classify` for the authoritative mapping — this RFC adds no new classes.

---

## §5 — SDK forwarder contract

After this RFC lands:

### §5.1 New SDK surface — `suspend` vs `try_suspend`

The SDK exposes two methods following the classic Rust `foo` / `try_foo` pattern (cf. `Lock` / `TryLock`, `RwLock::read` / `try_read`, `Mutex::lock` / `try_lock`):

```rust
// crates/ff-sdk/src/task.rs
impl ClaimedTask {
    /// **Strict suspend.** Always yields a Suspended handle or errors.
    /// Consumes `self`. On the early-satisfied path, returns
    /// `EngineError::State(AlreadySatisfied)` — the `ClaimedTask` is
    /// already gone, and the lease is released Lua-side as part of the
    /// error path. Use this when you've designed your flow to suspend
    /// unconditionally and an early-satisfied waitpoint is a bug.
    pub async fn suspend(
        self,
        reason_code: SuspensionReasonCode,
        resume_condition: ResumeCondition,
        timeout: Option<(TimestampMs, TimeoutBehavior)>,
        resume_policy: ResumePolicy,
    ) -> Result<SuspendedHandle, SdkError> {
        let handle = self.to_handle();
        let args = build_suspend_args(&self, reason_code, resume_condition, timeout, resume_policy);
        // NOTE: renewal is stopped only AFTER a successful Suspend outcome.
        // On transport error the lease continues to be renewed so the caller's
        // describe-and-reconcile per §3.1 is tractable. Suspended outcomes
        // have lease-state=unowned in Valkey so the renewal loop is a no-op
        // post-commit; stopping it cleanly avoids a pointless heartbeat.
        match self.backend.suspend(&handle, args).await.map_err(SdkError::from)? {
            SuspendOutcome::Suspended { handle, suspension_id, waitpoint_id, waitpoint_key, waitpoint_token } => {
                self.stop_renewal();
                Ok(SuspendedHandle { handle, suspension_id, waitpoint_id, waitpoint_key, waitpoint_token })
            }
            SuspendOutcome::AlreadySatisfied { .. } => {
                // Lease is retained Lua-side on AlreadySatisfied; strict form
                // declines it as an error. Renewal is NOT stopped — the caller
                // will either re-claim or drop, and the lease naturally rolls.
                Err(SdkError::Engine(EngineError::State(StateKind::AlreadySatisfied)))
            }
        }
    }

    /// **Fallible suspend.** Returns `SuspendOutcome::{Suspended,
    /// AlreadySatisfied}`. On `AlreadySatisfied`, the `ClaimedTask`
    /// is **kept alive** and handed back to the caller so the worker
    /// can continue running against the existing lease. Use this when
    /// consuming a pending waitpoint whose buffered signals may
    /// already match the condition.
    pub async fn try_suspend(
        self,
        reason_code: SuspensionReasonCode,
        resume_condition: ResumeCondition,
        timeout: Option<(TimestampMs, TimeoutBehavior)>,
        resume_policy: ResumePolicy,
    ) -> Result<TrySuspendOutcome, SdkError> { /* ... */ }

    /// Convenience: `try_suspend` against a pending waitpoint previously
    /// issued via `create_waitpoint`.
    pub async fn try_suspend_on_pending(
        self,
        pending: &PendingWaitpoint,
        reason_code: SuspensionReasonCode,
        resume_condition: ResumeCondition,
        timeout: Option<(TimestampMs, TimeoutBehavior)>,
    ) -> Result<TrySuspendOutcome, SdkError> { /* mirrors above with UsePending */ }
}

pub enum TrySuspendOutcome {
    Suspended(SuspendedHandle),
    AlreadySatisfied { task: ClaimedTask, details: SuspendOutcomeDetails },
}

/// Shared "what happened on the waitpoint" bundle, carried in both the
/// strict `SuspendedHandle` and the `try_suspend` AlreadySatisfied arm.
pub struct SuspendOutcomeDetails {
    pub suspension_id: SuspensionId,
    pub waitpoint_id: WaitpointId,
    pub waitpoint_key: String,
    pub waitpoint_token: WaitpointHmac,
}
```

**Helper: `build_suspend_args`.** SDK-internal; mints UUID v4 for `suspension_id`, takes `waitpoint: WaitpointBinding` from the caller (strict `suspend` defaults to `WaitpointBinding::fresh()`; `try_suspend_on_pending` passes `WaitpointBinding::use_pending(...)`), splits `timeout` into `timeout_at`/`timeout_behavior` (with `TimeoutBehavior::Fail` when `timeout` is `None` and the `timeout_at` is itself `None`), reads `now` from `SystemTime::now().into()`, defaults `requested_by = Worker`, `continuation_metadata_pointer = None`, `idempotency_key = None`. Callers who want idempotent retry use a richer SDK entry point (not in this RFC's surface) that threads `IdempotencyKey` through.

**`PendingWaitpoint` → `WaitpointBinding::UsePending`.** `create_waitpoint` returns a `PendingWaitpoint` struct carrying `waitpoint_id: WaitpointId` plus the minted `WaitpointHmac` token. `WaitpointBinding::use_pending(&pending)` destructures just the `waitpoint_id`; the HMAC token is **resolved Lua-side** from the partition's waitpoint hash at `suspend` time. Round-tripping the token through Rust is redundant and was already rejected by RFC-004 §Waitpoint Security (token lives on the hash, not in worker memory).

**No SDK-level auto-reconcile.** The SDK `suspend` / `try_suspend` wrappers do **not** auto-retry on transport error. Callers own the describe-and-reconcile loop per §3.1; alternately, callers set `idempotency_key` on `SuspendArgs` (via the SDK entry point that threads it) to make the retry safe. This is intentional — auto-reconcile would hide the "is my exec suspended or not?" ambiguity from the consumer, which matters for observability and correctness at scale.

Rationale for the split:
- `suspend` / `try_suspend` is the idiomatic Rust pattern for "operation may fail in a structured, callable-recoverable way": the strict form returns the happy path directly, the try form returns a Result-of-outcome. `Mutex::lock` panics on poisoning; `Mutex::try_lock` returns `Result`. Same cadence.
- 95% of suspend sites know they want to suspend unconditionally. For them, the strict form is cleaner and exhaustiveness-checks to one variant.
- The 5% of sites that suspend on a pending waitpoint with possibly-buffered signals need to retain the lease when the early-signal path fires. `try_suspend` hands `ClaimedTask` back so the worker continues without a re-claim round-trip.
- Moves the "AlreadySatisfied consumes vs retains the task" decision into the API surface instead of being a behavioral surprise.

- `SuspendOutcome` is **re-exported** from `ff-core::backend` into `ff-sdk::task`. The SDK does not own the type.
- Handlers pattern-match `TrySuspendOutcome` (for `try_suspend`) or receive a `SuspendedHandle` directly (for `suspend`). No further wrapping.
- **Deletions**:
  - `ff-sdk::task::parse_suspend_result` (100 LOC). Gone.
  - `ff-sdk::task::ConditionMatcher` (the `struct { signal_name: String }`). Replaced by `ResumeCondition::Single { waitpoint_key, matcher: SignalMatcher::ByName(name) }` (or `SignalMatcher::Wildcard`).
  - `ff-sdk::task::TimeoutBehavior` (if SDK-local — confirm in impl; otherwise just re-export from `ff-core`).
  - The direct `self.client.fcall("ff_suspend_execution", …)` call site. Replaced by `self.backend.suspend(…)`.

### §5.2 Compatibility break

Signature-level SDK break. `ClaimedTask::suspend` goes from
`(reason_code: &str, &[ConditionMatcher], Option<u64>, TimeoutBehavior)`
to the shape in §5.1. All downstream callers (known: `ff-sdk` tests, `cairn-fabric` consumer) must update. Release notes:

> **Breaking — `ff-sdk::ClaimedTask::suspend`:** signature changed and split. `ClaimedTask::suspend` is now the strict form (returns `SuspendedHandle`; errors with `State(AlreadySatisfied)` on the early-satisfied branch). New `ClaimedTask::try_suspend` returns `TrySuspendOutcome::{Suspended, AlreadySatisfied{task, ..}}` and retains the `ClaimedTask` on `AlreadySatisfied`. Pass typed `ResumeCondition` and `ResumePolicy`. `ConditionMatcher` is removed; use `ResumeCondition::Single { waitpoint_key, matcher }` for single-name. Multi-name fan-out moves to RFC-014's `ResumeCondition::Composite(CompositeBody)`. Internal parser `parse_suspend_result` deleted.

Consumer migration is mechanical; `cairn-fabric` owns its own migration (peer-team boundary per memory). The FF release produces a migration snippet but does not modify `cairn-fabric` code.

### §5.3 `Handle` substitution contract at call site

Under Stage 1d, §5.1's two-method split determines what happens to `ClaimedTask`:

- **`suspend` (strict), `Suspended` path**: `ClaimedTask` is consumed; caller receives a `SuspendedHandle` (opaque SDK post-suspension cookie). There is **no** public SDK type to "continue as suspended worker"; resumption goes through `claim_resumed_execution` which mints a fresh `ClaimedTask` on a later poll. The handle is only useful to `observe_signals` until that re-claim — preserving today's model.
- **`suspend` (strict), `AlreadySatisfied` path**: impossible by contract — the strict form errors with `EngineError::State(AlreadySatisfied)` and `self` has already been moved. Callers using `suspend` have declared early-satisfaction is a bug.
- **`try_suspend`, `Suspended` path**: same as strict. Caller gets a `SuspendedHandle`.
- **`try_suspend`, `AlreadySatisfied` path**: `ClaimedTask` is handed back in `TrySuspendOutcome::AlreadySatisfied { task, details }`. Lease is retained (Lua never released it); fence triple still matches. Worker continues against the original task. The waitpoint is closed with `satisfied=1` and the buffered signals are consumed per `resume_policy.consume_matched_signals`.

---

## §6 — Interactions

- **RFC-004 (Suspension)**: unchanged. This RFC re-shapes the Rust surface; the Valkey wire contract (Lua `ff_suspend_execution`, keys, ARGV, error codes) is preserved. Lua stays source of truth.
- **RFC-005 (Signal)**: unchanged. `deliver_signal` and `claim_resumed_execution` landed in PR #200 with typed args (`DeliverSignalArgs`, `ClaimResumedExecutionArgs`) — this RFC aligns `suspend`'s input shape with that precedent.
- **RFC-012 §R7.6.1**: this RFC is the answer to the pinned open question ("typed `ResumeCondition` on trait vs ARGV-JSON"). Decision: typed. See §8.2.
- **RFC-014 (multi-signal conditions, parallel draft)**: RFC-013 reserves `ResumeCondition::Composite(CompositeBody)`; RFC-014 defines `CompositeBody`. The flat `AnyOf`/`AllOf` variants an earlier draft proposed are dropped as a redundant subset of RFC-014's recursive composites (§2.4). RFC-014's `CompositeBody` is required to be `#[non_exhaustive]` and to take nested `ResumeCondition`s (not flat signal-name vecs) so subsequent RFCs can extend without breaking RFC-013's surface. RFC-014 may also add further top-level `ResumeCondition` variants (e.g. `Count`); none of that re-shapes the method.
- **Waitpoint HMAC protocol (RFC-004 §Waitpoint Security)**: unchanged. `WaitpointHmac` carries the token through typed fields. Minting remains Lua-side; validation remains Lua-side; rotation remains via `/v1/admin/rotate-waitpoint-secret`.
- **`create_waitpoint` (round-7, landed)**: `suspend` interoperates via `WaitpointBinding::UsePending`. The lua `use_pending_waitpoint` ARGV flag is the backend's serialization of that variant; no Lua change.
- **`observe_signals` / `claim_resumed_execution` (landed)**: unchanged. The post-suspend handle hands off to those methods exactly as today.

---

## §7 — Open questions (owner review required)

> **Adjudicated 2026-04-23** (owner): previous §7.2 (try_suspend split), §7.3 (drop flat `AnyOf`/`AllOf`), §7.5 (`idempotency_key` now) are answered and absorbed into §5 (SDK forwarder), §2.4 (`ResumeCondition`), and §2.2 (`SuspendArgs::idempotency_key`) respectively. Remaining open questions renumbered below.

### §7.1 `timeout: Option<(TimestampMs, TimeoutBehavior)>` vs split fields

The SDK `suspend` / `try_suspend` in §5.1 bundle `timeout_at` + `timeout_behavior` into one `Option<(…)>`. An alternative: two separate `Option<TimestampMs>` + `TimeoutBehavior` params with `TimeoutBehavior::Fail` as default.

- **Tradeoff**: tuple form prevents the "timeout_at set but timeout_behavior forgotten, behavior silently defaulted" bug. Split form reads nicer at the call site (`None` for timeout is clearer than `None` on a tuple).
- **Recommendation**: tuple form (catches the bug). Owner: pick.
- The trait-level `SuspendArgs` shape is unaffected either way — this is an SDK ergonomics question.

### §7.2 `SuspensionRequester` default

SDK callers always pass `SuspensionRequester::Worker`. Do we need the field on `SuspendArgs` at all, or is it implicit (worker-originated)?
- Operator-originated suspends (e.g. "admin pauses this exec") go through a different path (not `ClaimedTask::suspend` — there is no lease to surrender). So `suspend` as a trait op is always worker-requested in v1.
- **Recommendation**: keep the field; RFC-004 has `paused_by_policy` and `system_timeout_policy` reasons that imply non-worker requesters will eventually flow through this method. Owner: confirm.

### §7.3 `continuation_metadata_pointer` — string or typed?

Today it's a free-form string. Proposal: keep as `Option<String>`. An alternative: typed `ContinuationPointer { scheme: String, path: String }` for slightly better self-describability at no enforcement cost (the engine still doesn't interpret bytes).
- **Recommendation**: keep `Option<String>`. The engine has zero semantics to enforce on the contents; typing it is ceremony. Owner: confirm.

---

## §8 — Alternatives rejected

### §8.1 `SuspendOutcome` as a struct with `Option<Handle>` rather than an enum

```rust
struct SuspendOutcome {
    suspension_id: SuspensionId,
    waitpoint_id: WaitpointId,
    waitpoint_key: String,
    waitpoint_token: WaitpointHmac,
    handle: Option<Handle>,  // None = AlreadySatisfied; Some = Suspended
    already_satisfied: bool,
}
```

**Rejected** because:
- Pairs that should be exclusive (`handle.is_none() ⇔ already_satisfied`) get inconsistent over time.
- SDK call sites pattern-match on `already_satisfied`, not the Option — opens the door for "I forgot to check and unwrapped the handle on AS". Runtime panic instead of compile error.
- Enum variants let `#[non_exhaustive]` guard RFC-014 additions cleanly; a struct with an extra `timeout_elapsed: bool` field is exactly the footgun pattern.

### §8.2 ARGV-JSON in the backend impl, keeping a stringly-typed `ResumeCondition: serde_json::Value` on the trait

Round-7 §R7.6.1's alternative phrasing. **Rejected** because:
- Pushes the schema into no-lint land. No rust-level check that `Single.matcher.by_name` isn't misspelled `byName`, or that `Composite.all_of` isn't `allOf`.
- A Postgres backend can't produce the same JSON without cross-team sync.
- RFC-014's additions become string-format RFCs instead of trait additions.

### §8.3 Keep today's stub path (ship nothing)

**Rejected** because it leaves `dyn EngineBackend::suspend` permanently broken. RFC-012's Stage-1 gate requires every op forward through the trait. Postponing into RFC-014 is worse — two trait breaks back-to-back against downstream consumers.

### §8.4 Split `suspend` into `suspend_fresh` and `suspend_on_pending`

Mirror the WaitpointBinding variants at the trait level. **Rejected** because:
- Adds one more trait method for no semantic win.
- The backend op is exactly one Lua FCALL (`ff_suspend_execution` with `use_pending_waitpoint` ARGV flag); trait surface should match the op count.
- SDK ergonomics can still offer two methods (§5.1 shows `suspend` + `suspend_on_pending`) — the split belongs there, not at the trait.

### §8.5 Consume `Handle` instead of borrowing (Round-4 M-D2 reversal)

**Rejected** because M-D2 locked borrowing in Round-4, and the `AlreadySatisfied` branch is the exact reason borrow-not-consume wins: on AS, the caller's handle is still live. Consuming would force the AS variant to re-materialize a Handle, which is strictly worse.

### §8.6 Return `Result<(Handle, SuspendMetadata), EngineError>` (struct beside the handle)

A flatter shape: always return a Handle, with metadata in a side struct, and encode "AlreadySatisfied" in the `HandleKind` (e.g. new `HandleKind::LeaseRetainedAfterSuspend`). **Rejected** because:
- Pollutes `HandleKind` with a transient, single-op-specific kind.
- `LeaseRetained` would have to be reconverted to `Fresh` on the next caller op. More bookkeeping, not less.
- The enum outcome (§2.3) matches Round-7's `ClaimResumedExecutionResult` shape (one enum for op-specific outcomes). Consistency.

---

## §9 — Implementation plan

### §9.1 Landing order

1. **ff-core types** (§2.2 – §2.6): land `SuspendArgs`, `SuspendOutcome`, `WaitpointBinding`, `ResumeCondition`, `ResumePolicy`, `ResumeTarget`, `TimeoutBehavior`, `SuspensionReasonCode`, `SuspensionRequester` in `crates/ff-core/src/contracts/mod.rs` (and re-export from `ff-core::backend`). Zero behavioral change; just types.
2. **Trait update** (§2.1): change `EngineBackend::suspend` signature in `crates/ff-core/src/engine_backend.rs`. Breaking change on the trait. Stubbed impl remains `EngineError::Unavailable { op: "suspend" }` — still compiles.
3. **Valkey backend impl**: fill in `crates/ff-backend-valkey/src/lib.rs:~1240-1249`. Build KEYS (17) / ARGV (17) from `SuspendArgs`. Internal `ResumeCondition` → JSON serializer. Parse Lua return into `SuspendOutcome` — move `parse_suspend_result` logic into the backend, not SDK.
4. **SDK forwarder** (§5.1): rewrite `ClaimedTask::suspend` as the strict wrapper and add `ClaimedTask::try_suspend` / `try_suspend_on_pending`. Introduce `SuspendedHandle` and `TrySuspendOutcome`. Delete `parse_suspend_result`, `ConditionMatcher`, SDK-local `TimeoutBehavior` if present.
5. **Release notes + migration snippet** for 0.6.0.

### §9.2 Lua changes

**Scoped to the `idempotency_key` dedup path.** The non-idempotent path of `ff_suspend_execution` (KEYS 1..17, ARGV 1..17 as shipped) is preserved verbatim — the backend serializer materializes the same strings today's SDK builds by hand. Zero wire risk on that path.

**New dedup-path delta:**

- **+1 KEY (slot 18):** `ff:dedup:suspend:{p:N}:<execution_id>:<idempotency_key>` — a partition-scoped hash keyed on the triple from §2.2. Omitted (empty string passthrough) when the caller does not supply an `idempotency_key`.
- **+2 ARGV:**
  - **slot 18:** `idempotency_key` string (empty when `None`).
  - **slot 19:** `dedup_ttl_ms` u64 — the TTL the backend computes per §2.2 (`min(timeout_at - now + SUSPEND_DEDUP_GRACE_MS, SUSPEND_DEDUP_MAX_TTL_MS)`; `SUSPEND_DEDUP_MAX_TTL_MS` when `timeout_at` is `None`).
- **Lua changes to `ff_suspend_execution`:**
  1. Early branch: if ARGV[18] is non-empty, `HGET` the dedup key at KEYS[18] field `outcome`. If present, decode and return verbatim — skip all state mutation.
  2. Happy path: run existing logic unchanged.
  3. Post-commit: if ARGV[18] is non-empty, serialize the computed `SuspendOutcome` (using the existing positional-array return format — length-prefixed fields, same layout `parse_suspend_result` reads today) into the dedup hash's `outcome` field, then `PEXPIRE` KEYS[18] by `ARGV[19]`.
- **Serialized-outcome stability.** The dedup hash stores the Lua-side positional-array return bytes verbatim — the same bytes the script returns on a fresh call. This means dedup format evolves atomically with the `ff_suspend_execution` return contract; a future Lua upgrade that changes the return shape must version-gate dedup reads (recommended: leading version byte). RFC-013 pins `v=1` (no version byte, matches today's shape); future changes are RFC-014+ scope.
- **`resume_delay_ms` wire path.** Verified against RFC-004: `resume_delay_ms` is carried today in the existing `resume_policy_json` ARGV slot (within the 17-ARGV envelope), not on a separate slot. No new ARGV position needed for this field; the backend serializer writes it into `resume_policy_json` as today.
- **No change to HMAC / kid-ring / waitpoint-security paths.** Assumption A2 preserved.

Total new Lua LOC: ~30 inside `ff_suspend_execution`. Keys/ARGV slot counts: 17 → 18 / 17 → 19. Backend `slot_count` assertions must update in lockstep with this RFC's landing.

### §9.3 Test matrix

Required tests (at the landing PR's CI):

1. **Unit tests on the Rust type layer**:
   - `ResumeCondition::Single { matcher: SignalMatcher::Wildcard, .. }` round-trips.
   - `ResumeCondition::Single { matcher: SignalMatcher::ByName("") }` is rejected at Rust-side validation (empty name is not wildcard; wildcard is a dedicated variant).
   - `SuspendArgs` serde round-trip (JSON) for every `ResumeCondition` variant (RFC-013 surface: `Single`, `OperatorOnly`, `TimeoutOnly`; `Composite` covered at RFC-014 landing).
   - `SuspendOutcome::Suspended`/`AlreadySatisfied` exhaustiveness check in the SDK `try_suspend` wrapper; strict `suspend` wrapper mapping `AlreadySatisfied → State(AlreadySatisfied)` covered by one test.
   - `SuspendArgs::idempotency_key` replay test: two calls with the same key return the same outcome; two calls with different keys act independently (second errors with `State(AlreadySuspended)`).
   - `SuspendArgs::idempotency_key` TTL-expiry test: call, advance server-time past `dedup_ttl_ms`, third call with the same key acts fresh (errors `State(AlreadySuspended)` because exec is still suspended — but the dedup hash has aged out, so this is a state-check, not a dedup-hit).
   - `SuspendArgs::idempotency_key` with `timeout_at == None` test: confirms TTL is capped at `SUSPEND_DEDUP_MAX_TTL_MS = 7 days`.
   - Cross-field validation: `WaitpointBinding::Fresh { waitpoint_key: "wpk:a" }` + `ResumeCondition::Single { waitpoint_key: "wpk:b" }` → `Validation(InvalidInput { detail: "waitpoint_key_mismatch" })`.
   - `ResumeCondition::TimeoutOnly` + `timeout_at.is_none()` → `Validation(InvalidInput { detail: "timeout_only_without_deadline" })`.
   - Constructor round-trip: `SuspendArgs::new(...).with_timeout(...).with_idempotency_key(...)` produces the same serialized form as manual struct construction (once the struct construction exists via `ff-core`'s impl).
   - `WaitpointBinding::fresh()` produces a `waitpoint_key` of form `wpk:<uuid-v4>` matching the UUID in `waitpoint_id`.
   - `ResumePolicy::normal()` has the v1-documented defaults.

2. **Valkey-backend integration tests**:
   - `suspend` with `WaitpointBinding::Fresh`, `ResumeCondition::Signal`, no timeout → `Suspended`.
   - `suspend` with `WaitpointBinding::UsePending`, no early signals → `Suspended`.
   - `suspend` with `WaitpointBinding::UsePending`, buffered signal already matches → `AlreadySatisfied`, lease retained (verify via `describe_execution` that lifecycle still `active`, lease unchanged).
   - `suspend` after fence-triple drift → `Contention(LeaseConflict)`.
   - `suspend` on already-suspended exec → `State(AlreadySuspended)`.
   - `suspend` with `timeout_at` in the past → `Validation(InvalidInput)`.
   - `suspend` with `ResumeCondition::Single { matcher: ByName("a") }` + `a` delivered → resumes; unrelated signal → stays suspended.
   - Multi-signal `Composite` cases are covered under RFC-014's test matrix (RFC-013 reserves the slot but doesn't exercise the body).
   - `suspend` with `TimeoutBehavior::Fail` + timeout scanner tick → exec terminal, `close_reason = timed_out_fail`.
   - `suspend` with `TimeoutBehavior::AutoResumeWithTimeoutSignal` + timeout → exec runnable, close_reason `timed_out_auto_resume`.
   - Replay: two `suspend` calls with same args → first succeeds, second returns `State(AlreadySuspended)` (not idempotent).

3. **Cross-backend shape tests**: `SuspendArgs` / `SuspendOutcome` are backend-agnostic. Add a mock `EngineBackend` in test scaffolding that accepts the typed args and produces both outcome variants — assert SDK forwarder dispatches correctly on each.

   - **Backend-internal `ResumeCondition → ARGV-JSON` serializer tests**: for each `ResumeCondition` variant landed by RFC-013 (`Single`, `OperatorOnly`, `TimeoutOnly`), assert the emitted ARGV JSON matches byte-for-byte the string the legacy SDK produced for the equivalent input. This is the canary that keeps the "Lua unchanged on the non-idempotent path" claim honest across future refactors.

   - **Scanner interaction**: existing RFC-004 scanner tests cover all `TimeoutBehavior` variants (`Fail`, `Cancel`, `Expire`, `AutoResumeWithTimeoutSignal`). RFC-013 does not change the scanner; rewire the existing scanner tests onto the typed `SuspendArgs` path to confirm no regression. `TimeoutBehavior::Escalate` stays v2-scoped per RFC-004 Implementation Notes and is not asserted at Stage 1d.

4. **Regression coverage**: the existing `ff-sdk` suspension integration tests rewire onto the new signature. No new fixtures needed.

### §9.4 Risk analysis

- **Breaking SDK signature**: single consumer (`cairn-fabric`). Owned by peer team. Migration snippet in release notes.
- **Wire contract preserved**: zero Lua risk, zero data-model risk.
- **Type surface growth**: 9 new public types in `ff-core`. All `#[non_exhaustive]`; extending them does not break consumers.
- **`parse_suspend_result` deletion**: removes a 100-LOC parser duplicating the Lua return contract. Strictly removes technical debt.

### §9.5 Rollback

If Stage 1d is reverted mid-release:
- Types stay in `ff-core` (harmless).
- Trait signature reverts to Round-4 shape.
- Backend impl reverts to `Unavailable` stub.
- SDK forwarder reverts to direct FCALL + `parse_suspend_result`.

No durable state schema changes. Clean revert.

---

## References

- RFC-004 (Suspension) — canonical semantics for waitpoints, resume conditions, timeouts.
- RFC-005 (Signal) — signal semantics that satisfy resume conditions.
- RFC-012 §R7.6.1 — the open question this RFC closes.
- PR #200 — `deliver_signal` + `claim_resumed_execution` trait migration (precedent for typed args shape).
- `crates/ff-core/src/contracts/mod.rs` — landing site for new types (matches `DeliverSignalArgs` pattern at line 722).
- `crates/ff-core/src/engine_backend.rs:157-162` — trait method being re-shaped.
- `crates/ff-backend-valkey/src/lib.rs:~1240-1249` — current stub.
- `crates/ff-sdk/src/task.rs:812-924, 1448-1547` — SDK forwarder + parser, both migrated.
