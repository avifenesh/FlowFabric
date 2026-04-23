//! ScriptError-aware extension helpers for `ff_core::EngineError`.
//!
//! **RFC-012 Stage 1a:** the enum types live in ff-core so the
//! `EngineBackend` trait can name them without ff-core depending on
//! ff-script. The ScriptError-downcast logic cannot live in ff-core
//! (naming `ScriptError` would require ff-core → ff-script, wrong
//! direction). This module owns:
//!
//! * `impl From<ScriptError> for EngineError` — the full mapping.
//! * [`transport_script`] / [`transport_script_ref`] — construct a
//!   Valkey-tagged Transport variant from a `ScriptError` and downcast
//!   the boxed source back to `ScriptError` respectively.
//! * [`valkey_kind`] — inspect the inner `ferriskey::ErrorKind` when
//!   the Transport variant carries a `ScriptError::Valkey`.
//! * [`class`] — upgrade ff-core's default `Terminal` classification
//!   of Transport into the correct Phase-1 classification by
//!   delegating to `ScriptError::class`.

use crate::error::ScriptError;
use ff_core::engine_error::{
    BugKind, ConflictKind, ContentionKind, EngineError, StateKind, ValidationKind,
};
use ff_core::error::ErrorClass;

/// Construct a Valkey-backed `Transport` from a [`ScriptError`].
/// Preferred over struct-literal construction so the `backend` tag
/// stays consistent across Valkey call sites.
pub fn transport_script(err: ScriptError) -> EngineError {
    EngineError::Transport {
        backend: "valkey",
        source: Box::new(err),
    }
}

/// If `err` is a `Transport` carrying a [`ScriptError`], return a
/// reference to the inner script error. Returns `None` for other
/// variants and for Transport whose inner source is not a
/// `ScriptError` (e.g. a Postgres-backed transport error).
pub fn transport_script_ref(err: &EngineError) -> Option<&ScriptError> {
    match err {
        EngineError::Transport { source, .. } => source.downcast_ref::<ScriptError>(),
        // Descend through `backend_context(...)` wrappers so callers
        // looking for the inner `ScriptError` recover it.
        EngineError::Contextual { source, .. } => transport_script_ref(source),
        _ => None,
    }
}

/// Returns the underlying ferriskey [`ErrorKind`] if `err` maps back
/// to a transport-level fault whose inner source is a [`ScriptError`].
/// Postgres-backed or other non-Valkey transport errors return `None`.
///
/// [`ErrorKind`]: ferriskey::ErrorKind
pub fn valkey_kind(err: &EngineError) -> Option<ferriskey::ErrorKind> {
    match err {
        EngineError::Transport { source, .. } => source
            .downcast_ref::<ScriptError>()
            .and_then(|s| s.valkey_kind()),
        // Descend through `backend_context(...)` wrappers.
        EngineError::Contextual { source, .. } => valkey_kind(source),
        _ => None,
    }
}

/// Classification of `err` using the ScriptError-aware table.
///
/// Delegates to `EngineError::class()` for non-Transport variants;
/// for `Transport`, downcasts the inner source to `ScriptError` and
/// delegates to `ScriptError::class()`. Non-Valkey transport errors
/// (no `ScriptError` inside) default to `Terminal` — retrying an
/// unknown-shape error is unsafe.
pub fn class(err: &EngineError) -> ErrorClass {
    match err {
        EngineError::Transport { source, .. } => source
            .downcast_ref::<ScriptError>()
            .map(|s| s.class())
            .unwrap_or(ErrorClass::Terminal),
        // Descend into context-wrapped errors so ScriptError-aware
        // classification survives `backend_context(...)` wraps.
        EngineError::Contextual { source, .. } => class(source),
        other => other.class(),
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
            // `ff_sdk::engine_error::enrich_dependency_conflict` at
            // the stage_dependency site.
            S::DependencyAlreadyExists => transport_script(S::DependencyAlreadyExists),
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
            e @ (S::Parse { .. } | S::Valkey(_)) => transport_script(e),

            // `ScriptError` is `#[non_exhaustive]`. A future variant
            // landed in ff-script before the mapping here was updated
            // falls through to `Transport` with the raw ScriptError
            // preserved — strict-parse posture: caller still sees the
            // underlying error without a silent Display-string
            // downgrade. Adding the explicit variant later is a
            // non-breaking mapping refinement.
            other => transport_script(other),
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
        let err = EngineError::from(ScriptError::DependencyAlreadyExists);
        match &err {
            EngineError::Transport { backend, source } => {
                assert_eq!(*backend, "valkey");
                assert!(matches!(
                    source.downcast_ref::<ScriptError>(),
                    Some(ScriptError::DependencyAlreadyExists)
                ));
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
        let err = EngineError::from(ScriptError::Parse {
            fcall: "test_fn".into(),
            execution_id: None,
            message: "bad envelope".into(),
        });
        match &err {
            EngineError::Transport { backend, source } => {
                assert_eq!(*backend, "valkey");
                assert!(matches!(
                    source.downcast_ref::<ScriptError>(),
                    Some(ScriptError::Parse { .. })
                ));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn transport_script_helper_round_trips() {
        let err = transport_script(ScriptError::AttemptNotFound);
        assert!(matches!(
            transport_script_ref(&err),
            Some(ScriptError::AttemptNotFound)
        ));
        assert_eq!(class(&err), ScriptError::AttemptNotFound.class());
    }

    #[test]
    fn transport_preserves_valkey_kind() {
        let src = ScriptError::Valkey(ferriskey::Error::from((
            ferriskey::ErrorKind::IoError,
            "boom",
        )));
        let err = EngineError::from(src);
        assert_eq!(valkey_kind(&err), Some(ferriskey::ErrorKind::IoError));
    }

    #[test]
    fn class_transport_delegates() {
        let err = EngineError::from(ScriptError::Valkey(ferriskey::Error::from((
            ferriskey::ErrorKind::IoError,
            "x",
        ))));
        assert_eq!(class(&err), ErrorClass::Retryable);
    }

    #[test]
    fn class_transport_with_non_script_source_terminal() {
        let raw = std::io::Error::other("simulated postgres net error");
        let err = EngineError::Transport {
            backend: "postgres",
            source: Box::new(raw),
        };
        assert_eq!(class(&err), ErrorClass::Terminal);
        assert!(valkey_kind(&err).is_none());
        assert!(transport_script_ref(&err).is_none());
    }

    #[test]
    fn unavailable_has_no_script_source() {
        let err = EngineError::Unavailable { op: "claim" };
        assert_eq!(class(&err), ErrorClass::Terminal);
        assert!(valkey_kind(&err).is_none());
        assert!(transport_script_ref(&err).is_none());
    }
}
