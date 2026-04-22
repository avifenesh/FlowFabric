//! Typed engine-error surface (issue #58.6).
//!
//! **RFC-012 Stage 1a:** moved from `ff-sdk::engine_error` to
//! `ff-core::engine_error` so it becomes nameable by the
//! `EngineBackend` trait (which lives in `ff-core::engine_backend`) without
//! forcing a public-surface dependency from ff-core on ff-script. The
//! [`ScriptError`]-aware helpers (`From<ScriptError>`, `valkey_kind`,
//! `transport_script`, `transport_script_ref`) live in ff-script as
//! free functions (see `ff_script::engine_error_ext`) — ff-core owns
//! the enum shapes; ff-script owns the transport-downcast plumbing.
//!
//! # Mapping shape
//!
//! `ScriptError` lives in the `ff-script` crate (transport-adjacent).
//! `EngineError` lives here in `ff-core` and is what public SDK calls
//! return via `ff_sdk::SdkError::Engine`. The bidirectional mapping:
//!
//! * `From<ScriptError> for EngineError` — every `ScriptError` variant
//!   is classified into `NotFound` / `Validation` / `Contention` /
//!   `Conflict` / `State` / `Bug` / `Transport`. `Parse` + `Valkey`
//!   flow through `Transport { source: Box<ScriptError> }` so the
//!   underlying `ferriskey::ErrorKind` / parse detail is preserved.
//! * `DependencyAlreadyExists` is special: per the #58.6 design the
//!   variant carries the pre-existing [`EdgeSnapshot`] inline.
//!   Populating that field requires an extra round-trip (the Lua
//!   script only knows the edge_id), so plain `From<ScriptError>`
//!   returns a `Transport` fallback for that code — callers in the
//!   `stage_dependency` path use `ff_sdk::engine_error::enrich_dependency_conflict`
//!   to perform the follow-up `describe_edge` and upgrade the error
//!   before returning.
//!
//! # Exhaustiveness
//!
//! The top-level [`EngineError`] and every sub-kind are
//! `#[non_exhaustive]`. FF can add new Lua error codes in minors
//! without a breaking change to this surface — consumers that
//! `match` on a sub-kind must include a `_` arm.

use crate::error::ErrorClass;

/// Typed engine-error surface. See module docs.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EngineError {
    /// A uniquely-identified resource did not exist. `entity` is a
    /// stable label (e.g. `"execution"`, `"flow"`, `"attempt"`) that
    /// consumers can match without re-parsing a message.
    #[error("not found: {entity}")]
    NotFound { entity: &'static str },

    /// Caller supplied a malformed, out-of-range, or otherwise
    /// rejected input. `detail` carries the Lua-side payload (field
    /// name, offending value, or CSV of missing tokens, depending on
    /// `kind`).
    #[error("validation: {kind:?}: {detail}")]
    Validation {
        kind: ValidationKind,
        detail: String,
    },

    /// Transient conflict with another worker or with the current
    /// state of the execution/flow. Caller should retry per
    /// RFC-010 §10.7.
    #[error("contention: {0:?}")]
    Contention(ContentionKind),

    /// Permanent conflict — the requested mutation conflicts with
    /// an existing record (e.g. duplicate edge, cycle, already-in-flow).
    /// Caller must not blindly retry.
    #[error("conflict: {0:?}")]
    Conflict(ConflictKind),

    /// Legal but surprising state — lease expired, already-suspended,
    /// duplicate-signal, budget-exceeded, etc. Per-variant semantics
    /// documented on [`StateKind`].
    #[error("state: {0:?}")]
    State(StateKind),

    /// FF-internal invariant violation that should not be reachable
    /// in a correctly-behaving deployment. Consumers typically log
    /// and surface as a 5xx.
    #[error("bug: {0:?}")]
    Bug(BugKind),

    /// Backend transport fault or response-parse failure (RFC-012 §4.2
    /// round-4 shape). Broadened in Stage 0 to carry `Box<dyn Error>`
    /// so non-Valkey backends (Postgres, future) can route their
    /// native transport errors through this variant without going via
    /// `ScriptError`.
    ///
    /// * `backend` — static diagnostic label (`"valkey"`, `"postgres"`,
    ///   etc.). Kept `&'static str` to avoid heap alloc on construction.
    /// * `source` — boxed error. For the Valkey backend this is
    ///   `ff_script::error::ScriptError`; downcast with
    ///   `source.downcast_ref::<ScriptError>()` to recover
    ///   `ferriskey::ErrorKind` / parse detail. Helper lives in
    ///   `ff_script::engine_error_ext::transport_script_ref`.
    #[error("transport ({backend}): {source}")]
    Transport {
        backend: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },

    /// Backend method not wired up yet (RFC-012 §4.2 K#7 holdover).
    /// Returned by staged backend impls for methods that are known
    /// types in the trait but not yet implemented. Graceful degradation
    /// in place of `unimplemented!()` panics. Additive; does not
    /// participate in the `From<ScriptError>` mapping.
    #[error("unavailable: {op}")]
    Unavailable { op: &'static str },
}

/// Validation sub-kinds. 1:1 with the Lua validation codes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValidationKind {
    /// Generic caller-supplied input rejected (field-name detail).
    InvalidInput,
    /// Worker caps do not satisfy execution's required_capabilities.
    /// `detail` is the sorted-CSV of missing tokens.
    CapabilityMismatch,
    /// Malformed/oversized capability list.
    InvalidCapabilities,
    /// `policy_json` not valid JSON or structurally wrong.
    InvalidPolicyJson,
    /// Signal payload > 64KB.
    PayloadTooLarge,
    /// Max signals per execution reached.
    SignalLimitExceeded,
    /// MAC verification failed on waitpoint_key.
    InvalidWaitpointKey,
    /// Pending waitpoint has no HMAC token field.
    WaitpointNotTokenBound,
    /// Frame > 64KB.
    RetentionLimitExceeded,
    /// Lease/attempt binding mismatch on suspend.
    InvalidLeaseForSuspend,
    /// Dependency edge not found / invalid dependency ref.
    InvalidDependency,
    /// Waitpoint/execution binding mismatch.
    InvalidWaitpointForExecution,
    /// Unrecognized blocking reason.
    InvalidBlockingReason,
    /// Invalid stream ID offset.
    InvalidOffset,
    /// Auth failed.
    Unauthorized,
    /// Budget scope malformed.
    InvalidBudgetScope,
    /// Operator privileges required.
    BudgetOverrideNotAllowed,
    /// Malformed quota definition.
    InvalidQuotaSpec,
    /// Rotation kid must be non-empty and dot-free.
    InvalidKid,
    /// Rotation secret must be non-empty even-length hex.
    InvalidSecretHex,
    /// Rotation grace_ms must be a non-negative integer.
    InvalidGraceMs,
    /// Tag key violates reserved-namespace rule.
    InvalidTagKey,
    /// Unrecognized stream frame type.
    InvalidFrameType,
}

/// Contention sub-kinds (retryable per RFC-010 §10.7). Caller should
/// re-dispatch or re-read and retry.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContentionKind {
    /// Re-dispatch to `claim_resumed_execution`.
    UseClaimResumedExecution,
    /// Re-dispatch to `claim_execution`.
    NotAResumedExecution,
    /// State changed since grant. Request new grant.
    ExecutionNotLeaseable,
    /// Another worker holds lease. Request a different execution.
    LeaseConflict,
    /// Grant missing/mismatched. Request new grant.
    InvalidClaimGrant,
    /// Grant TTL elapsed. Request new grant.
    ClaimGrantExpired,
    /// No execution currently available.
    NoEligibleExecution,
    /// Waitpoint may not exist yet. Retry with backoff.
    WaitpointNotFound,
    /// Route to buffer_signal_for_pending_waitpoint.
    WaitpointPendingUseBufferScript,
    /// Graph revision changed. Re-read adjacency, retry.
    StaleGraphRevision,
    /// Execution is not in `active` state (lease superseded, etc.)
    /// Carries the Lua-side detail payload for replay reconciliation.
    ExecutionNotActive {
        terminal_outcome: String,
        lease_epoch: String,
        lifecycle_phase: String,
        attempt_id: String,
    },
    /// State changed. Scheduler skips.
    ExecutionNotEligible,
    /// Removed by another scheduler.
    ExecutionNotInEligibleSet,
    /// Already reclaimed/cancelled. Skip.
    ExecutionNotReclaimable,
    /// Target has no active lease (already revoked/expired/unowned).
    NoActiveLease,
    /// Window full; caller should backoff `retry_after_ms`.
    RateLimitExceeded,
    /// Concurrency cap hit.
    ConcurrencyLimitExceeded,
}

/// Permanent conflict sub-kinds. Caller must reconcile rather than
/// retry.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConflictKind {
    /// Dependency edge already exists. Carries the pre-existing
    /// [`EdgeSnapshot`] so callers implementing "409 on re-declare
    /// with different kind/ref" don't need a follow-up read.
    ///
    /// Note: the plain `From<ScriptError> for EngineError` impl
    /// cannot populate `existing` (that requires an async
    /// `describe_edge` round trip), so it falls through to
    /// `EngineError::Transport`. Callers on the `stage_dependency`
    /// path use `ff_sdk::engine_error::enrich_dependency_conflict`
    /// to perform the follow-up read and promote the error.
    ///
    /// [`EdgeSnapshot`]: crate::contracts::EdgeSnapshot
    DependencyAlreadyExists { existing: crate::contracts::EdgeSnapshot },
    /// Edge would create a cycle.
    CycleDetected,
    /// Self-referencing edge (upstream == downstream).
    SelfReferencingEdge,
    /// Execution is already a member of another flow.
    ExecutionAlreadyInFlow,
    /// Waitpoint already exists (pending or active).
    WaitpointAlreadyExists,
    /// Budget already attached or conflicts.
    BudgetAttachConflict,
    /// Quota policy already attached.
    QuotaAttachConflict,
    /// Rotation: same kid already installed with a different secret.
    /// String is the conflicting kid.
    RotationConflict(String),
    /// Invariant violation: active attempt already exists where one
    /// was expected absent.
    ActiveAttemptExists,
}

/// Legal-but-surprising state sub-kinds. Per-variant semantics vary
/// (some are benign no-ops, some are terminal). Consult the RFC-010
/// §10.7 classification table.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StateKind {
    /// Lease superseded by reclaim.
    StaleLease,
    /// Lease TTL elapsed.
    LeaseExpired,
    /// Operator revoked lease.
    LeaseRevoked,
    /// Already resumed/cancelled. No-op.
    ExecutionNotSuspended,
    /// Open suspension already active. No-op.
    AlreadySuspended,
    /// Signal too late — waitpoint already closed.
    WaitpointClosed,
    /// Execution not suspended; no valid signal target.
    TargetNotSignalable,
    /// Signal already delivered (dedup).
    DuplicateSignal,
    /// Resume conditions not satisfied.
    ResumeConditionNotMet,
    /// Waitpoint not in pending state.
    WaitpointNotPending,
    /// Pending waitpoint aged out before suspension committed.
    PendingWaitpointExpired,
    /// Waitpoint is not in an open state.
    WaitpointNotOpen,
    /// Cannot replay non-terminal execution.
    ExecutionNotTerminal,
    /// Replay limit reached.
    MaxReplaysExhausted,
    /// Attempt terminal; no appends.
    StreamClosed,
    /// Lease mismatch on stream append.
    StaleOwnerCannotAppend,
    /// Grant already issued. Skip.
    GrantAlreadyExists,
    /// Execution not in specified flow.
    ExecutionNotInFlow,
    /// Flow already in terminal state.
    FlowAlreadyTerminal,
    /// Dependencies not yet satisfied.
    DepsNotSatisfied,
    /// Not blocked by dependencies.
    NotBlockedByDeps,
    /// Execution not runnable.
    NotRunnable,
    /// Execution already terminal.
    Terminal,
    /// Hard budget limit reached.
    BudgetExceeded,
    /// Soft budget limit reached (warning; continue).
    BudgetSoftExceeded,
    /// Usage seq already processed. No-op.
    OkAlreadyApplied,
    /// Attempt not in started state.
    AttemptNotStarted,
    /// Attempt already ended. No-op.
    AttemptAlreadyTerminal,
    /// Wrong state for new attempt.
    ExecutionNotEligibleForAttempt,
    /// Execution not terminal or replay limit reached.
    ReplayNotAllowed,
    /// Retry limit reached.
    MaxRetriesExhausted,
    /// Already closed. No-op.
    StreamAlreadyClosed,
}

/// FF-internal invariant-violation sub-kinds. Should not be reachable
/// in a correctly-behaving deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BugKind {
    /// `attempt_not_in_created_state`: internal sequencing error.
    AttemptNotInCreatedState,
}

impl EngineError {
    /// Classify an [`EngineError`] using the underlying
    /// [`ErrorClass`] table.
    ///
    /// **Transport classification in ff-core:** the inner source is
    /// `Box<dyn std::error::Error>` which ff-core cannot downcast
    /// without naming `ScriptError`. ff-core returns `Terminal` for
    /// every `Transport` variant by default. Callers needing the
    /// Retryable-on-transient-Valkey-error classification use
    /// `ff_script::engine_error_ext::class` which downcasts to
    /// `ScriptError` and delegates to `ScriptError::class`. ff-sdk's
    /// public `SdkError::is_retryable` / `valkey_kind` methods wire
    /// the ff-script helper in so consumers retain the Phase-1
    /// behavior transparently.
    pub fn class(&self) -> ErrorClass {
        match self {
            Self::NotFound { .. } => ErrorClass::Terminal,
            Self::Validation { .. } => ErrorClass::Terminal,
            Self::Contention(_) => ErrorClass::Retryable,
            Self::Conflict(_) => ErrorClass::Terminal,
            Self::State(StateKind::BudgetExceeded) => ErrorClass::Cooperative,
            Self::State(
                StateKind::ExecutionNotSuspended
                | StateKind::AlreadySuspended
                | StateKind::WaitpointClosed
                | StateKind::DuplicateSignal
                | StateKind::GrantAlreadyExists
                | StateKind::OkAlreadyApplied
                | StateKind::AttemptAlreadyTerminal
                | StateKind::StreamAlreadyClosed
                | StateKind::BudgetSoftExceeded
                | StateKind::WaitpointNotOpen
                | StateKind::WaitpointNotPending
                | StateKind::PendingWaitpointExpired
                | StateKind::NotBlockedByDeps
                | StateKind::DepsNotSatisfied,
            ) => ErrorClass::Informational,
            Self::State(_) => ErrorClass::Terminal,
            Self::Bug(_) => ErrorClass::Bug,
            // ff-core cannot name ScriptError. Safe default: Terminal.
            // ff-script's engine_error_ext::class upgrades to
            // ScriptError::class when the inner source is a
            // ScriptError.
            Self::Transport { .. } => ErrorClass::Terminal,
            // Unavailable is terminal at the call site — the method is
            // not implemented; the caller must either fall back to a
            // different code path or surface to the user.
            Self::Unavailable { .. } => ErrorClass::Terminal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_contention_is_retryable() {
        let err = EngineError::Contention(ContentionKind::LeaseConflict);
        assert_eq!(err.class(), ErrorClass::Retryable);
    }

    #[test]
    fn class_budget_exceeded_is_cooperative() {
        let err = EngineError::State(StateKind::BudgetExceeded);
        assert_eq!(err.class(), ErrorClass::Cooperative);
    }

    #[test]
    fn class_duplicate_signal_is_informational() {
        let err = EngineError::State(StateKind::DuplicateSignal);
        assert_eq!(err.class(), ErrorClass::Informational);
    }

    #[test]
    fn class_bug_variant() {
        let err = EngineError::Bug(BugKind::AttemptNotInCreatedState);
        assert_eq!(err.class(), ErrorClass::Bug);
    }

    #[test]
    fn class_transport_defaults_terminal() {
        // ff-core has no ScriptError downcast; Transport is Terminal
        // until ff-script's engine_error_ext::class is called.
        let raw = std::io::Error::other("simulated transport error");
        let err = EngineError::Transport {
            backend: "test",
            source: Box::new(raw),
        };
        assert_eq!(err.class(), ErrorClass::Terminal);
    }

    #[test]
    fn unavailable_is_terminal() {
        assert_eq!(
            EngineError::Unavailable { op: "foo" }.class(),
            ErrorClass::Terminal
        );
    }
}
