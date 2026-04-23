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

    /// An inner [`EngineError`] wrapped with a call-site label so
    /// operators triaging logs can see which op the error came from
    /// without inferring from surrounding spans. Constructed via
    /// [`backend_context`]; carries a lightweight string context
    /// (e.g. `"renew: FCALL ff_renew_lease"`).
    ///
    /// Classification helpers (`ErrorClass`, `BackendErrorKind`,
    /// etc.) transparently descend into `source` so a consumer that
    /// matches on the wrapper arm keeps the same retry/terminal
    /// semantics as the unwrapped inner error.
    #[error("{context}: {source}")]
    Contextual {
        #[source]
        source: Box<EngineError>,
        context: String,
    },
}

/// Wrap an [`EngineError`] with a call-site label when the error is
/// a transport-family fault — `Transport` or `Unavailable`. Typed
/// classifications (`NotFound`, `Validation`, `Contention`,
/// `Conflict`, `State`, `Bug`) form the public contract boundary
/// for consumers that `match` on the variant, so we return them
/// unchanged. Repeated wraps on an already-`Contextual` error
/// nest an additional layer; callers should wrap once per op
/// boundary.
///
/// Promoted to ff-core so `ff-backend-valkey` can annotate its
/// `EngineBackend` impls with the same context shape ff-sdk's
/// snapshot helpers use (issue #154).
pub fn backend_context(err: EngineError, context: impl Into<String>) -> EngineError {
    match err {
        EngineError::Transport { .. }
        | EngineError::Unavailable { .. }
        | EngineError::Contextual { .. } => EngineError::Contextual {
            source: Box::new(err),
            context: context.into(),
        },
        // Typed classifications are part of the public contract;
        // wrapping them would break `match` call sites that inspect
        // the inner variant (e.g. tests asserting
        // `EngineError::Validation { kind: Corruption, .. }`).
        other => other,
    }
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
    /// On-disk corruption or protocol drift: an engine-owned hash /
    /// key returned a field shape the decoder could not parse (missing
    /// required field, malformed timestamp, unknown extra field,
    /// cross-field identity mismatch, etc.). `detail` carries the
    /// decoder's diagnostic string — the specific field name and/or
    /// offending value — in the form
    /// `"<context>: <field?>: <message>"` so operators can locate the
    /// bad key without reparsing.
    ///
    /// Classified as `Terminal`: a consumer retrying the read will
    /// see the same bytes. Surface to the operator; do not loop.
    Corruption,
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
    DependencyAlreadyExists {
        existing: crate::contracts::EdgeSnapshot,
    },
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
    /// RFC-013 Stage 1d — strict `suspend` path refuses the
    /// early-satisfied branch. The underlying backend outcome is
    /// [`crate::contracts::SuspendOutcome::AlreadySatisfied`]; only the
    /// SDK's strict `ClaimedTask::suspend` wrapper maps it to this
    /// error. `ClaimedTask::try_suspend` returns the outcome directly.
    AlreadySatisfied,
}

/// FF-internal invariant-violation sub-kinds. Should not be reachable
/// in a correctly-behaving deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BugKind {
    /// `attempt_not_in_created_state`: internal sequencing error.
    AttemptNotInCreatedState,
}

/// Backend-agnostic transport error carried across public
/// ff-sdk / ff-server error surfaces (#88).
///
/// The `Valkey` variant is the only one populated today; additional
/// variants (e.g. `Postgres`) will be added additively as other
/// backends land. The enum is `#[non_exhaustive]` so consumers must
/// include a wildcard arm.
///
/// Construction from the Valkey-native `ferriskey::Error` lives in
/// `ff_backend_valkey::backend_error_from_ferriskey` — keeping that
/// conversion outside ff-core preserves ff-core's ferriskey-free
/// public surface.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum BackendError {
    /// Valkey-backend transport failure. Carries a backend-agnostic
    /// classification plus the backend-rendered message so downstream
    /// consumers can inspect without depending on ferriskey.
    #[error("valkey backend: {kind:?}: {message}")]
    Valkey {
        kind: BackendErrorKind,
        message: String,
    },
}

impl BackendError {
    /// Returns the classified backend kind if this error is a Valkey
    /// transport fault. Forward-compatible with future backends:
    /// non-Valkey variants return `None` on a call that names only the
    /// Valkey kind; code that wants a backend-specific view should
    /// match directly on [`BackendError`].
    pub fn kind(&self) -> BackendErrorKind {
        match self {
            Self::Valkey { kind, .. } => *kind,
        }
    }

    /// Return the backend-rendered message payload.
    pub fn message(&self) -> &str {
        match self {
            Self::Valkey { message, .. } => message.as_str(),
        }
    }
}

/// Classified backend transport errors, kept backend-agnostic on
/// purpose (#88). Each variant maps a family of native backend error
/// kinds into a stable, consumer-matchable shape.
///
/// Consumers requiring the exact native kind for a Valkey backend
/// must go through `ff_backend_valkey` explicitly; ff-sdk/ff-server's
/// public surface will only ever hand out [`BackendErrorKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackendErrorKind {
    /// Network / I/O failure: the request may or may not have been
    /// processed. Typically retryable with backoff.
    Transport,
    /// Backend rejected the request on protocol / parse grounds. Not
    /// retryable without a fix.
    Protocol,
    /// Backend timed out responding to the request. Retryable.
    Timeout,
    /// Authentication / authorization failure. Not retryable.
    Auth,
    /// Cluster topology churn (MOVED, ASK, CLUSTERDOWN, MasterDown,
    /// CrossSlot, ConnectionNotFoundForRoute, AllConnectionsUnavailable).
    /// Retryable after topology settles.
    Cluster,
    /// Backend is temporarily busy loading state (e.g. Valkey
    /// `LOADING`). Retryable.
    BusyLoading,
    /// Backend indicates the referenced script/function does not
    /// exist. Typically handled by the caller via re-load.
    ScriptNotLoaded,
    /// Any other classified error from the backend. Fallback bucket
    /// for native kinds outside the curated set above.
    Other,
}

impl BackendErrorKind {
    /// Stable, lowercase-kebab label suitable for log fields / HTTP
    /// `kind` body slots. Guaranteed not to change across releases
    /// for the existing variants.
    pub fn as_stable_str(&self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Protocol => "protocol",
            Self::Timeout => "timeout",
            Self::Auth => "auth",
            Self::Cluster => "cluster",
            Self::BusyLoading => "busy_loading",
            Self::ScriptNotLoaded => "script_not_loaded",
            Self::Other => "other",
        }
    }

    /// Whether a caller should consider this kind retryable with
    /// backoff. Conservative — auth + protocol + other are terminal.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Transport | Self::Timeout | Self::Cluster | Self::BusyLoading
        )
    }
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
    /// public `SdkError::is_retryable` / `backend_kind` methods wire
    /// the ff-script helper in so consumers retain the Phase-1
    /// behavior transparently. (`backend_kind` was renamed from
    /// `valkey_kind` in #88.)
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
                | StateKind::AlreadySatisfied
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
            // Descend into the wrapped error — context is diagnostic;
            // classification follows the inner cause.
            Self::Contextual { source, .. } => source.class(),
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

    #[test]
    fn backend_context_wraps_transport_and_preserves_typed() {
        // Transport gets wrapped with the call-site label (issue #154).
        let raw = std::io::Error::other("simulated transport error");
        let wrapped = backend_context(
            EngineError::Transport {
                backend: "valkey",
                source: Box::new(raw),
            },
            "renew: FCALL ff_renew_lease",
        );
        let rendered = format!("{wrapped}");
        assert!(
            rendered.starts_with("renew: FCALL ff_renew_lease: transport (valkey): "),
            "expected context prefix, got: {rendered}"
        );
        // Unavailable also wraps so callers can still filter on the op.
        let wrapped = backend_context(EngineError::Unavailable { op: "x" }, "ctx");
        assert!(matches!(wrapped, EngineError::Contextual { .. }));

        // Typed classifications pass through unchanged so existing
        // `match` call sites keep working.
        let inner = EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: "bad".into(),
        };
        let passthrough = backend_context(inner, "describe_edge: HGETALL edge");
        match passthrough {
            EngineError::Validation { kind, .. } => {
                assert_eq!(kind, ValidationKind::Corruption);
            }
            other => panic!("expected Validation, got {other:?}"),
        }
        let inner = EngineError::Contention(ContentionKind::LeaseConflict);
        assert_eq!(
            backend_context(inner, "renew: FCALL ff_renew_lease").class(),
            ErrorClass::Retryable
        );
    }

    #[test]
    fn backend_error_kind_round_trip() {
        let be = BackendError::Valkey {
            kind: BackendErrorKind::Transport,
            message: "connection reset".into(),
        };
        assert_eq!(be.kind(), BackendErrorKind::Transport);
        assert_eq!(be.message(), "connection reset");
    }

    #[test]
    fn backend_kind_stable_strings_fixed() {
        // Stability fence: these strings are part of the public
        // contract (log field values, HTTP body `kind` slots). Adding
        // a variant is additive; changing an existing string is a
        // break.
        assert_eq!(BackendErrorKind::Transport.as_stable_str(), "transport");
        assert_eq!(BackendErrorKind::Protocol.as_stable_str(), "protocol");
        assert_eq!(BackendErrorKind::Timeout.as_stable_str(), "timeout");
        assert_eq!(BackendErrorKind::Auth.as_stable_str(), "auth");
        assert_eq!(BackendErrorKind::Cluster.as_stable_str(), "cluster");
        assert_eq!(
            BackendErrorKind::BusyLoading.as_stable_str(),
            "busy_loading"
        );
        assert_eq!(
            BackendErrorKind::ScriptNotLoaded.as_stable_str(),
            "script_not_loaded"
        );
        assert_eq!(BackendErrorKind::Other.as_stable_str(), "other");
    }

    #[test]
    fn backend_kind_retryability() {
        for k in [
            BackendErrorKind::Transport,
            BackendErrorKind::Timeout,
            BackendErrorKind::Cluster,
            BackendErrorKind::BusyLoading,
        ] {
            assert!(k.is_retryable(), "{k:?} should be retryable");
        }
        for k in [
            BackendErrorKind::Protocol,
            BackendErrorKind::Auth,
            BackendErrorKind::ScriptNotLoaded,
            BackendErrorKind::Other,
        ] {
            assert!(!k.is_retryable(), "{k:?} should NOT be retryable");
        }
    }
}
