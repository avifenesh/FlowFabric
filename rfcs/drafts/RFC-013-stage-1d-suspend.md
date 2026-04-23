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
    /// Single signal-name match. Matches the SDK's v1 `ConditionMatcher`
    /// with `signal_name == ""` meaning wildcard.
    Signal {
        /// Empty = wildcard (any signal name).
        signal_name: String,
    },

    /// "any of these signal names" — matches when one signal in the
    /// set arrives.
    AnyOf { signal_names: Vec<String> },

    /// "all of these signal names" — matches when every name has
    /// arrived at least once. (RFC-004 §Resume Condition fields already
    /// describes `all` as v1.)
    AllOf { signal_names: Vec<String> },

    /// Operator-only resume — no signal ever satisfies; only an
    /// explicit operator resume closes. Used for escalate-style paths.
    OperatorOnly,

    /// Timeout-only — waitpoint has no signal satisfier, only the
    /// `timeout_behavior` at `timeout_at` resolves it. (Useful for
    /// pure-delay suspensions where a signal arriving would be an
    /// error, not a resumer.)
    TimeoutOnly,
}
```

**Forward-compat hooks (RFC-014 will extend):**
- `#[non_exhaustive]` lets RFC-014 add `Count { signal_name, n }`, `CompositeAll { clauses: Vec<ResumeCondition> }`, `CompositeAny { clauses }` without breaking downstream pattern matches.
- Nesting is expressed by making the enum self-recursive in future variants — `AllOf` today takes `Vec<String>` (flat); RFC-014's `CompositeAll` takes `Vec<ResumeCondition>` (nested). Both coexist — `AllOf` is the flat-signal-name fast-path, `CompositeAll` is the RFC-014 general form. §7.3 tracks whether to collapse `AllOf` into `CompositeAll` at RFC-014 landing time.
- `minimum_signal_count` (RFC-004 §Resume Condition Fields) is NOT a top-level `SuspendArgs` field; it is implicit per variant — `AllOf` ⇒ `n = |signal_names|`; `AnyOf` ⇒ `n = 1`; `Count` (RFC-014) will carry an explicit `n`.

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

2. **Same handle, same args, exec is in `suspended` already (discovered via describe).**
   - Caller should `claim_resumed_execution` or wait for signal — not re-`suspend`.

### §3.2 Invalid replay cases

3. **Second `suspend` on the fresh Suspended-kind handle returned by a successful `suspend`.**
   - `handle.kind == Suspended`. Backend pattern-matches: if kind is Suspended and the caller is trying to transition exec from `active` to `suspended`, return `EngineError::State(StateKind::AlreadySuspended)`. This is a programming bug; it is also cheap to detect before any FCALL (handle kind check in Rust).

4. **Second `suspend` with a new `suspension_id` after the first succeeded.**
   - Equivalent to (3) — the exec is already suspended. `EngineError::State(StateKind::AlreadySuspended)` via Lua `already_suspended` error code.

### §3.3 Contract summary

- `suspend` is **not** retry-idempotent. Retry requires a describe-and-reconcile dance. The `suspension_id` is echoed in `SuspendOutcome` for caller correlation, not for Lua-side dedup.
- A future enhancement (out of scope): add an optional `idempotency_key` on `SuspendArgs` backed by a dedup hash scan, analogous to `DeliverSignalArgs::idempotency_key`. Tracked as §7.5.

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
| `resume_condition == AnyOf { signal_names: [] }` or similar empty-condition footgun | `Validation(InvalidInput { detail: "resume_condition.signal_names" })` | Rust-side check before FCALL. |
| `timeout_at` in the past | `Validation(InvalidInput { detail: "timeout_at_in_past" })` | Rust-side. |
| HMAC secrets not initialised for partition | `Transport(boxed ScriptError::hmac_secret_not_initialized)` | Ops issue, not caller issue. |
| Valkey connection error / transient | `Transport(…)` | Standard. |

**New kinds needed:** none. Every scenario maps to an existing variant after Round-7. The `InvalidLeaseForSuspend` variant was added in Round-4 specifically for this path.

**Classification note:** `LeaseConflict` + `ExecutionNotActive` are retryable; `AlreadySuspended` is cooperative (no-op — the caller's intent is already the state); everything else is either terminal (validation, bug) or transport (standard retry). See `EngineError::classify` for the authoritative mapping — this RFC adds no new classes.

---

## §5 — SDK forwarder contract

After this RFC lands:

### §5.1 New SDK surface

```rust
// crates/ff-sdk/src/task.rs
impl ClaimedTask {
    pub async fn suspend(
        self,
        reason_code: SuspensionReasonCode,
        resume_condition: ResumeCondition,
        timeout: Option<(TimestampMs, TimeoutBehavior)>,
        resume_policy: ResumePolicy,
    ) -> Result<SuspendOutcome, SdkError> {
        let handle = self.to_handle();
        let args = SuspendArgs {
            suspension_id: SuspensionId::new(),
            waitpoint: WaitpointBinding::Fresh {
                waitpoint_id: WaitpointId::new(),
                waitpoint_key: format!("wpk:{}", WaitpointId::new()),
            },
            resume_condition,
            resume_policy,
            reason_code,
            requested_by: SuspensionRequester::Worker,
            timeout_at: timeout.map(|(t, _)| t),
            timeout_behavior: timeout.map(|(_, b)| b).unwrap_or(TimeoutBehavior::Fail),
            continuation_metadata_pointer: None,
            now: TimestampMs::now(),
        };
        self.stop_renewal();
        self.backend.suspend(&handle, args).await.map_err(SdkError::from)
    }

    /// Convenience: suspend with a pending waitpoint previously
    /// issued via `create_waitpoint`.
    pub async fn suspend_on_pending(
        self,
        pending: &PendingWaitpoint,
        reason_code: SuspensionReasonCode,
        resume_condition: ResumeCondition,
        timeout: Option<(TimestampMs, TimeoutBehavior)>,
    ) -> Result<SuspendOutcome, SdkError> { /* mirrors above with UsePending */ }
}
```

- `SuspendOutcome` is **re-exported** from `ff-core::backend` into `ff-sdk::task`. The SDK does not own the type.
- Handlers pattern-match `SuspendOutcome` directly. No further wrapping — the enum is the SDK contract.
- **Deletions**:
  - `ff-sdk::task::parse_suspend_result` (100 LOC). Gone.
  - `ff-sdk::task::ConditionMatcher` (the `struct { signal_name: String }`). Replaced by `ResumeCondition::Signal`.
  - `ff-sdk::task::TimeoutBehavior` (if SDK-local — confirm in impl; otherwise just re-export from `ff-core`).
  - The direct `self.client.fcall("ff_suspend_execution", …)` call site. Replaced by `self.backend.suspend(…)`.

### §5.2 Compatibility break

Signature-level SDK break. `ClaimedTask::suspend` goes from
`(reason_code: &str, &[ConditionMatcher], Option<u64>, TimeoutBehavior)`
to the shape in §5.1. All downstream callers (known: `ff-sdk` tests, `cairn-fabric` consumer) must update. Release notes:

> **Breaking — `ff-sdk::ClaimedTask::suspend`:** signature changed. Pass typed `ResumeCondition` and `ResumePolicy`. `ConditionMatcher` is removed; use `ResumeCondition::Signal { signal_name }` for single-name, `ResumeCondition::AnyOf { signal_names }` for the previous multi-name fan-out. Internal parser `parse_suspend_result` deleted.

Consumer migration is mechanical; `cairn-fabric` owns its own migration (peer-team boundary per memory). The FF release produces a migration snippet but does not modify `cairn-fabric` code.

### §5.3 `Handle` substitution contract at call site

Today's `ClaimedTask::suspend` consumes `self` — on any outcome, the task is gone. Under Stage 1d:

- `Suspended { handle, … }`: caller replaces their `ClaimedTask` with — nothing. `ClaimedTask` is consumed; the returned `Handle` belongs to an opaque SDK post-suspension cookie. There is **no** public SDK type to "continue as suspended worker"; resumption goes through `claim_resumed_execution` which mints a fresh `ClaimedTask` on a later poll. The `Handle` is only useful to `observe_signals` until that re-claim happens — which is how today's SDK already works. We preserve that exact model.
- `AlreadySatisfied { … }`: `ClaimedTask` is consumed but the lease is not released. The caller cannot continue running against the original task after an `AlreadySatisfied` outcome — the worker's current-task state is gone. This is a **breaking** behavioral carryover from today (`ClaimedTask::suspend` already consumes `self` in the AlreadySatisfied path). §7.2 proposes an owner-review alternative that preserves the task handle in `AlreadySatisfied`.

---

## §6 — Interactions

- **RFC-004 (Suspension)**: unchanged. This RFC re-shapes the Rust surface; the Valkey wire contract (Lua `ff_suspend_execution`, keys, ARGV, error codes) is preserved. Lua stays source of truth.
- **RFC-005 (Signal)**: unchanged. `deliver_signal` and `claim_resumed_execution` landed in PR #200 with typed args (`DeliverSignalArgs`, `ClaimResumedExecutionArgs`) — this RFC aligns `suspend`'s input shape with that precedent.
- **RFC-012 §R7.6.1**: this RFC is the answer to the pinned open question ("typed `ResumeCondition` on trait vs ARGV-JSON"). Decision: typed. See §8.2.
- **RFC-014 (multi-signal conditions, parallel draft)**: `ResumeCondition` is designed to extend with `Count`, `CompositeAll`, `CompositeAny`. RFC-014 adds variants; does not re-shape the method.
- **Waitpoint HMAC protocol (RFC-004 §Waitpoint Security)**: unchanged. `WaitpointHmac` carries the token through typed fields. Minting remains Lua-side; validation remains Lua-side; rotation remains via `/v1/admin/rotate-waitpoint-secret`.
- **`create_waitpoint` (round-7, landed)**: `suspend` interoperates via `WaitpointBinding::UsePending`. The lua `use_pending_waitpoint` ARGV flag is the backend's serialization of that variant; no Lua change.
- **`observe_signals` / `claim_resumed_execution` (landed)**: unchanged. The post-suspend handle hands off to those methods exactly as today.

---

## §7 — Open questions (owner review required)

### §7.1 `timeout: Option<(TimestampMs, TimeoutBehavior)>` vs split fields

The SDK `suspend` in §5.1 bundles `timeout_at` + `timeout_behavior` into one `Option<(…)>`. An alternative: two separate `Option<TimestampMs>` + `TimeoutBehavior` params with `TimeoutBehavior::Fail` as default.

- **Tradeoff**: tuple form prevents the "timeout_at set but timeout_behavior forgotten, behavior silently defaulted" bug. Split form reads nicer at the call site (`None` for timeout is clearer than `None` on a tuple).
- **Recommendation**: tuple form (catches the bug). Owner: pick.
- The trait-level `SuspendArgs` shape is unaffected either way — this is an SDK ergonomics question.

### §7.2 `AlreadySatisfied` — does `ClaimedTask` survive?

Today's `ClaimedTask::suspend` consumes `self` unconditionally. On `AlreadySatisfied` the lease is retained but the `ClaimedTask` is gone — the worker cannot continue running without some other path to reconstruct task state. In practice today this means: callers who get `AlreadySatisfied` must return control and let the execution be re-claimed, even though the lease is technically held.

Three options:
1. **Keep today's behavior** (consume `self` in both variants). Simple. Requires the worker to tear down and wait for re-claim. `AlreadySatisfied` is effectively a bug: the lease lingers until renew-on-a-dropped-task runs out.
2. **Return `Result<Either<(ClaimedTask, SuspendOutcome::AlreadySatisfied), SuspendOutcome::Suspended>, _>`** — on AlreadySatisfied, hand the `ClaimedTask` back so the worker continues. Needs the SDK wrapper method on ClaimedTask to be non-consuming in the AS branch, which is structurally awkward.
3. **Split into two SDK methods**: `ClaimedTask::suspend_strict` (consumes self, errors on AS) and `ClaimedTask::try_suspend` (returns `Either`). Strict is the 95% path; try is the pending-waitpoint early-signal path.
- **Recommendation**: option 3. Owner: pick. This is a call-site ergonomics question that doesn't affect the trait.

### §7.3 `AllOf` / `AnyOf` flat forms vs RFC-014's recursive `Composite*`

When RFC-014 lands its `CompositeAll { clauses: Vec<ResumeCondition> }`, the flat `AllOf { signal_names: Vec<String> }` becomes redundant. Two choices:

- Keep `AllOf` as a flat fast-path (documented as "equivalent to `CompositeAll { clauses: signal_names.map(Signal) }`"). Migration-free.
- Deprecate `AllOf` in RFC-014; remove in a later release. More churn, cleaner surface.
- **Recommendation**: keep flat form permanently. The backend serializer can internally collapse. Owner: pick, or defer until RFC-014.

### §7.4 `SuspensionRequester` default

SDK callers always pass `SuspensionRequester::Worker`. Do we need the field on `SuspendArgs` at all, or is it implicit (worker-originated)?
- Operator-originated suspends (e.g. "admin pauses this exec") go through a different path (not `ClaimedTask::suspend` — there is no lease to surrender). So `suspend` as a trait op is always worker-requested in v1.
- **Recommendation**: keep the field; RFC-004 has `paused_by_policy` and `system_timeout_policy` reasons that imply non-worker requesters will eventually flow through this method. Owner: confirm.

### §7.5 Optional `idempotency_key` on `SuspendArgs`

§3.3 flags that `suspend` is not retry-idempotent. Adding an `idempotency_key: Option<String>` field backed by a partition-scoped dedup hash (TTL = RFC-004 suspension-timeout ceiling) would make retry-after-network-error trivially safe at modest state cost.
- **Recommendation**: not in Stage 1d. Track as a follow-up; §3.1's describe-and-retry pattern suffices for v0.6. Owner: confirm deferral.

### §7.6 `continuation_metadata_pointer` — string or typed?

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
- Pushes the schema into no-lint land. No rust-level check that `AnyOf.signal_names` isn't misspelled `signalNames`.
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
4. **SDK forwarder** (§5.1): rewrite `ClaimedTask::suspend` to forward through the trait. Delete `parse_suspend_result`, `ConditionMatcher`, SDK-local `TimeoutBehavior` if present.
5. **Release notes + migration snippet** for 0.6.0.

### §9.2 Lua changes

**None required.** The wire contract (`ff_suspend_execution` KEYS/ARGV/return) is preserved. The backend serializer materializes the same ARGV strings today's SDK builds by hand. Zero Lua-level risk.

### §9.3 Test matrix

Required tests (at the landing PR's CI):

1. **Unit tests on the Rust type layer**:
   - `ResumeCondition::Signal { signal_name: "" }` round-trips as wildcard.
   - `ResumeCondition::AllOf { signal_names: [] }` is rejected at Rust-side validation.
   - `SuspendArgs` serde round-trip (JSON) for every `ResumeCondition` variant.
   - `SuspendOutcome::Suspended`/`AlreadySatisfied` exhaustiveness check in the SDK wrapper.

2. **Valkey-backend integration tests**:
   - `suspend` with `WaitpointBinding::Fresh`, `ResumeCondition::Signal`, no timeout → `Suspended`.
   - `suspend` with `WaitpointBinding::UsePending`, no early signals → `Suspended`.
   - `suspend` with `WaitpointBinding::UsePending`, buffered signal already matches → `AlreadySatisfied`, lease retained (verify via `describe_execution` that lifecycle still `active`, lease unchanged).
   - `suspend` after fence-triple drift → `Contention(LeaseConflict)`.
   - `suspend` on already-suspended exec → `State(AlreadySuspended)`.
   - `suspend` with `timeout_at` in the past → `Validation(InvalidInput)`.
   - `suspend` with `ResumeCondition::AnyOf { … }` + signal delivery that matches one name → resumes.
   - `suspend` with `ResumeCondition::AllOf { ["a", "b"] }` + only `a` delivered → stays suspended; + `b` delivered → resumes.
   - `suspend` with `TimeoutBehavior::Fail` + timeout scanner tick → exec terminal, `close_reason = timed_out_fail`.
   - `suspend` with `TimeoutBehavior::AutoResumeWithTimeoutSignal` + timeout → exec runnable, close_reason `timed_out_auto_resume`.
   - Replay: two `suspend` calls with same args → first succeeds, second returns `State(AlreadySuspended)` (not idempotent).

3. **Cross-backend shape tests**: `SuspendArgs` / `SuspendOutcome` are backend-agnostic. Add a mock `EngineBackend` in test scaffolding that accepts the typed args and produces both outcome variants — assert SDK forwarder dispatches correctly on each.

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
