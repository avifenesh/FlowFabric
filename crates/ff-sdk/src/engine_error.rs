//! Typed engine-error surface (issue #58.6).
//!
//! [`EngineError`] is the typed public error type that replaces
//! string-matching on [`ff_script::error::ScriptError`] codes. Every
//! `ScriptError` code maps 1:1 into a sub-kind variant — consumers
//! that today match on the FCALL error-envelope string (cairn's
//! `is_claim_contention`, `fcall_error_code`, etc.) can pattern-match
//! the typed enum instead.
//!
//! # Mapping shape
//!
//! `ScriptError` lives in the `ff-script` crate (transport-adjacent).
//! `EngineError` lives here in `ff-sdk` and is what public SDK calls
//! return via [`crate::SdkError::Engine`]. The bidirectional mapping:
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
//!   `stage_dependency` path use [`EngineError::enrich_dependency_conflict`]
//!   to perform the follow-up `describe_edge` and upgrade the error
//!   before returning.
//!
//! # Exhaustiveness
//!
//! The top-level [`EngineError`] and every sub-kind are
//! `#[non_exhaustive]`. FF can add new Lua error codes in minors
//! without a breaking change to this surface — consumers that
//! `match` on a sub-kind must include a `_` arm.

use std::collections::HashMap;

use ff_core::contracts::EdgeSnapshot;
use ff_core::error::ErrorClass;
use ff_core::keys::FlowKeyContext;
use ff_core::partition::flow_partition;
use ff_core::types::{EdgeId, FlowId};
use ff_script::error::ScriptError;

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

    /// Valkey transport fault or FCALL response-parse failure.
    /// Preserves the raw [`ScriptError`] via `Box` so callers can
    /// recover the `ferriskey::ErrorKind` (for `Valkey`) or the
    /// parse-site message (for `Parse`).
    #[error("transport: {source}")]
    Transport {
        #[source]
        source: Box<ScriptError>,
    },
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
    /// path use [`EngineError::enrich_dependency_conflict`] to
    /// perform the follow-up read and promote the error.
    DependencyAlreadyExists { existing: EdgeSnapshot },
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
    /// [`ErrorClass`] table. Delegates to the `ScriptError` mapping
    /// via [`ScriptError::class`] when the original is recoverable
    /// through `Transport`; otherwise uses the variant bucket.
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
            Self::Transport { source } => source.class(),
        }
    }

    /// Returns the underlying ferriskey ErrorKind if this error maps
    /// back to a transport-level fault.
    pub fn valkey_kind(&self) -> Option<ferriskey::ErrorKind> {
        match self {
            Self::Transport { source } => source.valkey_kind(),
            _ => None,
        }
    }

    /// Upgrade a bare `From<ScriptError>` translation of
    /// `dependency_already_exists` into a fully-typed
    /// [`ConflictKind::DependencyAlreadyExists`] by performing the
    /// follow-up `HGETALL` on the edge hash.
    ///
    /// Call sites that stage a dependency and receive
    /// [`EngineError::Transport`] whose inner is
    /// `ScriptError::DependencyAlreadyExists` use this helper to
    /// upgrade the error before surfacing. On follow-up failure the
    /// original `Transport` error is returned unchanged — the
    /// strict-parse posture means a half-populated `Conflict` is
    /// never constructed.
    pub async fn enrich_dependency_conflict(
        client: &ferriskey::Client,
        partition_config: &ff_core::partition::PartitionConfig,
        flow_id: &FlowId,
        edge_id: &EdgeId,
    ) -> Result<Self, Self> {
        let partition = flow_partition(flow_id, partition_config);
        let ctx = FlowKeyContext::new(&partition, flow_id);
        let edge_key = ctx.edge(edge_id);

        let raw: HashMap<String, String> = match client
            .cmd("HGETALL")
            .arg(&edge_key)
            .execute()
            .await
        {
            Ok(raw) => raw,
            Err(transport) => {
                // Follow-up read failed: fall back to the raw
                // ScriptError so diagnostic detail is not lost.
                return Err(Self::Transport {
                    source: Box::new(ScriptError::Valkey(transport)),
                });
            }
        };
        if raw.is_empty() {
            // Edge hash absent despite the Lua reporting
            // dependency_already_exists — corruption or a
            // race with a concurrent writer. Surface as the
            // raw ScriptError rather than fabricate a stub.
            return Err(Self::Transport {
                source: Box::new(ScriptError::DependencyAlreadyExists),
            });
        }
        match crate::snapshot::build_edge_snapshot_public(flow_id, edge_id, &raw) {
            Ok(existing) => Ok(Self::Conflict(ConflictKind::DependencyAlreadyExists {
                existing,
            })),
            Err(_e) => Err(Self::Transport {
                source: Box::new(ScriptError::DependencyAlreadyExists),
            }),
        }
    }
}

impl From<ScriptError> for EngineError {
    fn from(err: ScriptError) -> Self {
        use ScriptError as S;
        match err {
            // ── NotFound ──
            S::ExecutionNotFound => Self::NotFound {
                entity: "execution",
            },
            S::FlowNotFound => Self::NotFound { entity: "flow" },
            S::AttemptNotFound => Self::NotFound { entity: "attempt" },
            S::BudgetNotFound => Self::NotFound { entity: "budget" },
            S::QuotaPolicyNotFound => Self::NotFound {
                entity: "quota_policy",
            },
            S::StreamNotFound => Self::NotFound { entity: "stream" },

            // ── Validation (carries detail) ──
            S::InvalidInput(d) => Self::Validation {
                kind: ValidationKind::InvalidInput,
                detail: d,
            },
            S::CapabilityMismatch(d) => Self::Validation {
                kind: ValidationKind::CapabilityMismatch,
                detail: d,
            },
            S::InvalidCapabilities(d) => Self::Validation {
                kind: ValidationKind::InvalidCapabilities,
                detail: d,
            },
            S::InvalidPolicyJson(d) => Self::Validation {
                kind: ValidationKind::InvalidPolicyJson,
                detail: d,
            },
            S::InvalidTagKey(d) => Self::Validation {
                kind: ValidationKind::InvalidTagKey,
                detail: d,
            },
            // ── Validation (no detail payload) ──
            S::PayloadTooLarge => Self::Validation {
                kind: ValidationKind::PayloadTooLarge,
                detail: String::new(),
            },
            S::SignalLimitExceeded => Self::Validation {
                kind: ValidationKind::SignalLimitExceeded,
                detail: String::new(),
            },
            S::InvalidWaitpointKey => Self::Validation {
                kind: ValidationKind::InvalidWaitpointKey,
                detail: String::new(),
            },
            S::WaitpointNotTokenBound => Self::Validation {
                kind: ValidationKind::WaitpointNotTokenBound,
                detail: String::new(),
            },
            S::RetentionLimitExceeded => Self::Validation {
                kind: ValidationKind::RetentionLimitExceeded,
                detail: String::new(),
            },
            S::InvalidLeaseForSuspend => Self::Validation {
                kind: ValidationKind::InvalidLeaseForSuspend,
                detail: String::new(),
            },
            S::InvalidDependency => Self::Validation {
                kind: ValidationKind::InvalidDependency,
                detail: String::new(),
            },
            S::InvalidWaitpointForExecution => Self::Validation {
                kind: ValidationKind::InvalidWaitpointForExecution,
                detail: String::new(),
            },
            S::InvalidBlockingReason => Self::Validation {
                kind: ValidationKind::InvalidBlockingReason,
                detail: String::new(),
            },
            S::InvalidOffset => Self::Validation {
                kind: ValidationKind::InvalidOffset,
                detail: String::new(),
            },
            S::Unauthorized => Self::Validation {
                kind: ValidationKind::Unauthorized,
                detail: String::new(),
            },
            S::InvalidBudgetScope => Self::Validation {
                kind: ValidationKind::InvalidBudgetScope,
                detail: String::new(),
            },
            S::BudgetOverrideNotAllowed => Self::Validation {
                kind: ValidationKind::BudgetOverrideNotAllowed,
                detail: String::new(),
            },
            S::InvalidQuotaSpec => Self::Validation {
                kind: ValidationKind::InvalidQuotaSpec,
                detail: String::new(),
            },
            S::InvalidKid => Self::Validation {
                kind: ValidationKind::InvalidKid,
                detail: String::new(),
            },
            S::InvalidSecretHex => Self::Validation {
                kind: ValidationKind::InvalidSecretHex,
                detail: String::new(),
            },
            S::InvalidGraceMs => Self::Validation {
                kind: ValidationKind::InvalidGraceMs,
                detail: String::new(),
            },
            S::InvalidFrameType => Self::Validation {
                kind: ValidationKind::InvalidFrameType,
                detail: String::new(),
            },

            // ── Contention ──
            S::UseClaimResumedExecution => {
                Self::Contention(ContentionKind::UseClaimResumedExecution)
            }
            S::NotAResumedExecution => Self::Contention(ContentionKind::NotAResumedExecution),
            S::ExecutionNotLeaseable => Self::Contention(ContentionKind::ExecutionNotLeaseable),
            S::LeaseConflict => Self::Contention(ContentionKind::LeaseConflict),
            S::InvalidClaimGrant => Self::Contention(ContentionKind::InvalidClaimGrant),
            S::ClaimGrantExpired => Self::Contention(ContentionKind::ClaimGrantExpired),
            S::NoEligibleExecution => Self::Contention(ContentionKind::NoEligibleExecution),
            S::WaitpointNotFound => Self::Contention(ContentionKind::WaitpointNotFound),
            S::WaitpointPendingUseBufferScript => {
                Self::Contention(ContentionKind::WaitpointPendingUseBufferScript)
            }
            S::StaleGraphRevision => Self::Contention(ContentionKind::StaleGraphRevision),
            S::ExecutionNotActive {
                terminal_outcome,
                lease_epoch,
                lifecycle_phase,
                attempt_id,
            } => Self::Contention(ContentionKind::ExecutionNotActive {
                terminal_outcome,
                lease_epoch,
                lifecycle_phase,
                attempt_id,
            }),
            S::ExecutionNotEligible => Self::Contention(ContentionKind::ExecutionNotEligible),
            S::ExecutionNotInEligibleSet => {
                Self::Contention(ContentionKind::ExecutionNotInEligibleSet)
            }
            S::ExecutionNotReclaimable => {
                Self::Contention(ContentionKind::ExecutionNotReclaimable)
            }
            S::NoActiveLease => Self::Contention(ContentionKind::NoActiveLease),
            S::RateLimitExceeded => Self::Contention(ContentionKind::RateLimitExceeded),
            S::ConcurrencyLimitExceeded => {
                Self::Contention(ContentionKind::ConcurrencyLimitExceeded)
            }

            // ── Conflict ──
            // DependencyAlreadyExists needs a follow-up read to
            // populate `existing`. Plain `From` cannot do the
            // async read, so falls through to Transport with the
            // raw ScriptError preserved — callers enrich via
            // `EngineError::enrich_dependency_conflict` at the
            // stage_dependency site.
            S::DependencyAlreadyExists => Self::Transport {
                source: Box::new(S::DependencyAlreadyExists),
            },
            S::CycleDetected => Self::Conflict(ConflictKind::CycleDetected),
            S::SelfReferencingEdge => Self::Conflict(ConflictKind::SelfReferencingEdge),
            S::ExecutionAlreadyInFlow => Self::Conflict(ConflictKind::ExecutionAlreadyInFlow),
            S::WaitpointAlreadyExists => Self::Conflict(ConflictKind::WaitpointAlreadyExists),
            S::BudgetAttachConflict => Self::Conflict(ConflictKind::BudgetAttachConflict),
            S::QuotaAttachConflict => Self::Conflict(ConflictKind::QuotaAttachConflict),
            S::RotationConflict(kid) => Self::Conflict(ConflictKind::RotationConflict(kid)),
            S::ActiveAttemptExists => Self::Conflict(ConflictKind::ActiveAttemptExists),

            // ── State ──
            S::StaleLease => Self::State(StateKind::StaleLease),
            S::LeaseExpired => Self::State(StateKind::LeaseExpired),
            S::LeaseRevoked => Self::State(StateKind::LeaseRevoked),
            S::ExecutionNotSuspended => Self::State(StateKind::ExecutionNotSuspended),
            S::AlreadySuspended => Self::State(StateKind::AlreadySuspended),
            S::WaitpointClosed => Self::State(StateKind::WaitpointClosed),
            S::TargetNotSignalable => Self::State(StateKind::TargetNotSignalable),
            S::DuplicateSignal => Self::State(StateKind::DuplicateSignal),
            S::ResumeConditionNotMet => Self::State(StateKind::ResumeConditionNotMet),
            S::WaitpointNotPending => Self::State(StateKind::WaitpointNotPending),
            S::PendingWaitpointExpired => Self::State(StateKind::PendingWaitpointExpired),
            S::WaitpointNotOpen => Self::State(StateKind::WaitpointNotOpen),
            S::ExecutionNotTerminal => Self::State(StateKind::ExecutionNotTerminal),
            S::MaxReplaysExhausted => Self::State(StateKind::MaxReplaysExhausted),
            S::StreamClosed => Self::State(StateKind::StreamClosed),
            S::StaleOwnerCannotAppend => Self::State(StateKind::StaleOwnerCannotAppend),
            S::GrantAlreadyExists => Self::State(StateKind::GrantAlreadyExists),
            S::ExecutionNotInFlow => Self::State(StateKind::ExecutionNotInFlow),
            S::FlowAlreadyTerminal => Self::State(StateKind::FlowAlreadyTerminal),
            S::DepsNotSatisfied => Self::State(StateKind::DepsNotSatisfied),
            S::NotBlockedByDeps => Self::State(StateKind::NotBlockedByDeps),
            S::NotRunnable => Self::State(StateKind::NotRunnable),
            S::Terminal => Self::State(StateKind::Terminal),
            S::BudgetExceeded => Self::State(StateKind::BudgetExceeded),
            S::BudgetSoftExceeded => Self::State(StateKind::BudgetSoftExceeded),
            S::OkAlreadyApplied => Self::State(StateKind::OkAlreadyApplied),
            S::AttemptNotStarted => Self::State(StateKind::AttemptNotStarted),
            S::AttemptAlreadyTerminal => Self::State(StateKind::AttemptAlreadyTerminal),
            S::ExecutionNotEligibleForAttempt => {
                Self::State(StateKind::ExecutionNotEligibleForAttempt)
            }
            S::ReplayNotAllowed => Self::State(StateKind::ReplayNotAllowed),
            S::MaxRetriesExhausted => Self::State(StateKind::MaxRetriesExhausted),
            S::StreamAlreadyClosed => Self::State(StateKind::StreamAlreadyClosed),

            // ── Bug ──
            S::AttemptNotInCreatedState => Self::Bug(BugKind::AttemptNotInCreatedState),

            // ── Transport (preserves source for Parse/Valkey) ──
            e @ (S::Parse(_) | S::Valkey(_)) => Self::Transport {
                source: Box::new(e),
            },

            // `ScriptError` is `#[non_exhaustive]`. A future variant
            // landed in ff-script before the mapping here was updated
            // falls through to `Transport` with the raw ScriptError
            // preserved — strict-parse posture: caller still sees the
            // underlying error without a silent Display-string
            // downgrade. Adding the explicit variant later is a
            // non-breaking mapping refinement.
            other => Self::Transport {
                source: Box::new(other),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_mappings() {
        assert!(matches!(
            EngineError::from(ScriptError::ExecutionNotFound),
            EngineError::NotFound { entity: "execution" }
        ));
        assert!(matches!(
            EngineError::from(ScriptError::FlowNotFound),
            EngineError::NotFound { entity: "flow" }
        ));
    }

    #[test]
    fn validation_detail_preserved() {
        match EngineError::from(ScriptError::CapabilityMismatch("gpu,cuda".into())) {
            EngineError::Validation {
                kind: ValidationKind::CapabilityMismatch,
                detail,
            } => assert_eq!(detail, "gpu,cuda"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn contention_bucket() {
        assert!(matches!(
            EngineError::from(ScriptError::LeaseConflict),
            EngineError::Contention(ContentionKind::LeaseConflict)
        ));
        assert!(matches!(
            EngineError::from(ScriptError::UseClaimResumedExecution),
            EngineError::Contention(ContentionKind::UseClaimResumedExecution)
        ));
    }

    #[test]
    fn execution_not_active_detail_flows_through() {
        let src = ScriptError::ExecutionNotActive {
            terminal_outcome: "success".into(),
            lease_epoch: "3".into(),
            lifecycle_phase: "terminal".into(),
            attempt_id: "att-1".into(),
        };
        match EngineError::from(src) {
            EngineError::Contention(ContentionKind::ExecutionNotActive {
                terminal_outcome,
                lease_epoch,
                lifecycle_phase,
                attempt_id,
            }) => {
                assert_eq!(terminal_outcome, "success");
                assert_eq!(lease_epoch, "3");
                assert_eq!(lifecycle_phase, "terminal");
                assert_eq!(attempt_id, "att-1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn dependency_already_exists_falls_through_to_transport_without_enrich() {
        // Plain From cannot fill `existing` — it defers to Transport
        // so the caller can upgrade via enrich_dependency_conflict.
        match EngineError::from(ScriptError::DependencyAlreadyExists) {
            EngineError::Transport { source } => {
                assert!(matches!(*source, ScriptError::DependencyAlreadyExists));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn conflict_variants() {
        assert!(matches!(
            EngineError::from(ScriptError::CycleDetected),
            EngineError::Conflict(ConflictKind::CycleDetected)
        ));
        assert!(matches!(
            EngineError::from(ScriptError::ExecutionAlreadyInFlow),
            EngineError::Conflict(ConflictKind::ExecutionAlreadyInFlow)
        ));
        match EngineError::from(ScriptError::RotationConflict("kid-1".into())) {
            EngineError::Conflict(ConflictKind::RotationConflict(k)) => assert_eq!(k, "kid-1"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn state_variants() {
        assert!(matches!(
            EngineError::from(ScriptError::StaleLease),
            EngineError::State(StateKind::StaleLease)
        ));
        assert!(matches!(
            EngineError::from(ScriptError::BudgetExceeded),
            EngineError::State(StateKind::BudgetExceeded)
        ));
    }

    #[test]
    fn bug_variants() {
        assert!(matches!(
            EngineError::from(ScriptError::AttemptNotInCreatedState),
            EngineError::Bug(BugKind::AttemptNotInCreatedState)
        ));
    }

    #[test]
    fn transport_preserves_parse() {
        match EngineError::from(ScriptError::Parse("bad envelope".into())) {
            EngineError::Transport { source } => {
                assert!(matches!(*source, ScriptError::Parse(_)));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn transport_preserves_valkey_kind() {
        let src = ScriptError::Valkey(ferriskey::Error::from((
            ferriskey::ErrorKind::IoError,
            "boom",
        )));
        let err = EngineError::from(src);
        assert_eq!(err.valkey_kind(), Some(ferriskey::ErrorKind::IoError));
    }

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
    fn class_transport_delegates() {
        let err = EngineError::from(ScriptError::Valkey(ferriskey::Error::from((
            ferriskey::ErrorKind::IoError,
            "x",
        ))));
        assert_eq!(err.class(), ErrorClass::Retryable);
    }
}
