//! FlowFabric `ScriptError` — Lua error codes + transport errors.
//!
//! Lives in `ff-script` (not `ff-core`) because the `Valkey` variant wraps
//! `ferriskey::Error`, and `ff-core` stays pure of the transport client.
//! `ff-core` continues to own the `ErrorClass` enum.

use ff_core::error::ErrorClass;

#[cfg(feature = "valkey-client")]
use crate::retry::is_retryable_kind;

/// All error codes returned by FlowFabric Valkey Functions.
/// Matches RFC-010 §10.7 exactly.
///
/// Does not derive `Serialize`/`Deserialize`/`PartialEq`/`Eq`/`Hash` because the
/// `Valkey` variant wraps `ferriskey::Error`, which implements none of those.
/// Call sites compare via `matches!`/`.class()` rather than `==`, so this is
/// not a regression.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ScriptError {
    // ── Lease/Ownership errors ──
    /// Stop. Lease superseded by reclaim.
    #[error("stale_lease: lease superseded by reclaim")]
    StaleLease,

    /// Stop. Lease TTL elapsed.
    #[error("lease_expired: lease TTL elapsed")]
    LeaseExpired,

    /// Stop. Operator revoked.
    #[error("lease_revoked: operator revoked lease")]
    LeaseRevoked,

    /// Execution is not in `active` state. Carries enriched detail so the
    /// SDK can reconcile a replay of a terminal operation after a network
    /// drop: if `terminal_outcome` matches what the caller attempted AND
    /// `lease_epoch` matches their lease, the prior call committed and
    /// the retry should be treated as a successful replay. `lifecycle_phase`
    /// distinguishes `terminal` from `runnable` (retry-scheduled).
    /// `attempt_id` is the per-attempt replay guard (preserved on
    /// terminal, cleared on retry).
    #[error(
        "execution_not_active: lifecycle_phase={lifecycle_phase} terminal_outcome={terminal_outcome} lease_epoch={lease_epoch} attempt_id={attempt_id}"
    )]
    ExecutionNotActive {
        terminal_outcome: String,
        lease_epoch: String,
        lifecycle_phase: String,
        attempt_id: String,
    },

    /// Revoke target has no active lease (already revoked/expired/unowned).
    #[error("no_active_lease: target has no active lease")]
    NoActiveLease,

    /// RFC #58.5: a lease-bound FCALL was invoked with an empty
    /// `(lease_id, lease_epoch, attempt_id)` triple and the caller is not
    /// an allowlisted operator override. Worker-path callers must always
    /// pass the fence triple; unfenced callers must supply
    /// `source == "operator_override"` (terminal ops) or use a different
    /// FCALL (renew / suspend hard-reject with no override path).
    /// TERMINAL: the caller is structurally incorrect; retrying without a
    /// fence will not help.
    #[error("fence_required: lease fence triple is mandatory for this FCALL")]
    FenceRequired,

    /// RFC #58.5: the fence triple arrived with some but not all three
    /// fields populated. Programming error — either all of
    /// (lease_id, lease_epoch, attempt_id) must be set, or all three must
    /// be empty. TERMINAL.
    #[error("partial_fence_triple: lease_id/lease_epoch/attempt_id must be all set or all empty")]
    PartialFenceTriple,

    /// Bug. Active attempt already exists.
    #[error("active_attempt_exists: invariant violation")]
    ActiveAttemptExists,

    // ── Claim dispatch errors ──
    /// Re-dispatch to claim_resumed_execution.
    #[error("use_claim_resumed_execution: attempt_interrupted, use resume claim path")]
    UseClaimResumedExecution,

    /// Re-dispatch to claim_execution.
    #[error("not_a_resumed_execution: use normal claim path")]
    NotAResumedExecution,

    /// State changed since grant. Request new grant.
    #[error("execution_not_leaseable: state changed since grant")]
    ExecutionNotLeaseable,

    /// Another worker holds lease. Request different execution.
    #[error("lease_conflict: another worker holds lease")]
    LeaseConflict,

    /// Grant missing/mismatched. Request new grant.
    #[error("invalid_claim_grant: grant missing or mismatched")]
    InvalidClaimGrant,

    /// Grant TTL elapsed. Request new grant.
    #[error("claim_grant_expired: grant TTL elapsed")]
    ClaimGrantExpired,

    /// Backoff 100ms-1s, retry.
    #[error("no_eligible_execution: no execution available")]
    NoEligibleExecution,

    // ── Budget/Quota enforcement ──
    /// Immediate stop. Call fail_execution(budget_exceeded).
    #[error("budget_exceeded: hard budget limit reached")]
    BudgetExceeded,

    /// Log warning. Continue.
    #[error("budget_soft_exceeded: soft budget limit reached")]
    BudgetSoftExceeded,

    // ── Suspension/Signal errors ──
    /// Already resumed/cancelled. No-op.
    #[error("execution_not_suspended: already resumed or cancelled")]
    ExecutionNotSuspended,

    /// Open suspension exists. No-op.
    #[error("already_suspended: suspension already active")]
    AlreadySuspended,

    /// Signal too late. Return to caller.
    #[error("waitpoint_closed: waitpoint already closed")]
    WaitpointClosed,

    /// Waitpoint may not exist yet. Retry with backoff.
    #[error("waitpoint_not_found: waitpoint does not exist yet")]
    WaitpointNotFound,

    /// Execution not suspended, no pending waitpoint.
    #[error("target_not_signalable: no valid signal target")]
    TargetNotSignalable,

    /// Route to buffer_signal_for_pending_waitpoint.
    #[error("waitpoint_pending_use_buffer_script: route to buffer script")]
    WaitpointPendingUseBufferScript,

    /// Dedup. Return existing signal_id.
    #[error("duplicate_signal: signal already delivered")]
    DuplicateSignal,

    /// Payload > 64KB.
    #[error("payload_too_large: signal payload exceeds 64KB")]
    PayloadTooLarge,

    /// Max signals reached.
    #[error("signal_limit_exceeded: max signals per execution reached")]
    SignalLimitExceeded,

    /// MAC failed. Token invalid or expired.
    #[error("invalid_waitpoint_key: MAC verification failed")]
    InvalidWaitpointKey,

    /// Invalid lease for suspend.
    #[error("invalid_lease_for_suspend: lease/attempt binding mismatch")]
    InvalidLeaseForSuspend,

    /// Conditions not satisfied.
    #[error("resume_condition_not_met: resume conditions not satisfied")]
    ResumeConditionNotMet,

    /// Waitpoint not in pending state.
    #[error("waitpoint_not_pending: waitpoint is not in pending state")]
    WaitpointNotPending,

    /// Pending waitpoint expired before suspension committed.
    #[error("pending_waitpoint_expired: pending waitpoint aged out")]
    PendingWaitpointExpired,

    /// Waitpoint/execution binding mismatch.
    #[error("invalid_waitpoint_for_execution: waitpoint does not belong to execution")]
    InvalidWaitpointForExecution,

    /// Waitpoint already exists (pending or active).
    #[error("waitpoint_already_exists: waitpoint already exists")]
    WaitpointAlreadyExists,

    /// Waitpoint not in an open state.
    #[error("waitpoint_not_open: waitpoint is not pending or active")]
    WaitpointNotOpen,

    // ── Replay errors ──
    /// Cannot replay non-terminal.
    #[error("execution_not_terminal: cannot replay non-terminal execution")]
    ExecutionNotTerminal,

    /// Replay limit reached.
    #[error("max_replays_exhausted: replay limit reached")]
    MaxReplaysExhausted,

    // ── Stream errors ──
    /// Attempt terminal. No appends.
    #[error("stream_closed: attempt terminal, no appends allowed")]
    StreamClosed,

    /// Lease mismatch on stream append.
    #[error("stale_owner_cannot_append: lease mismatch on append")]
    StaleOwnerCannotAppend,

    /// Frame > 64KB.
    #[error("retention_limit_exceeded: frame exceeds size limit")]
    RetentionLimitExceeded,

    // ── Scheduling errors ──
    /// State changed. Scheduler skips.
    #[error("execution_not_eligible: state changed")]
    ExecutionNotEligible,

    /// Another scheduler got it. Skip.
    #[error("execution_not_in_eligible_set: removed by another scheduler")]
    ExecutionNotInEligibleSet,

    /// Grant already issued. Skip.
    #[error("grant_already_exists: grant already active")]
    GrantAlreadyExists,

    /// Already reclaimed/cancelled. Skip.
    #[error("execution_not_reclaimable: already reclaimed or cancelled")]
    ExecutionNotReclaimable,

    // ── Flow/Dependency errors ──
    /// Edge doesn't exist.
    #[error("invalid_dependency: dependency edge not found")]
    InvalidDependency,

    /// Re-read adjacency, retry.
    #[error("stale_graph_revision: graph has been updated")]
    StaleGraphRevision,

    /// Already in another flow.
    #[error("execution_already_in_flow: execution belongs to another flow")]
    ExecutionAlreadyInFlow,

    /// Edge would create cycle.
    #[error("cycle_detected: dependency edge would create cycle")]
    CycleDetected,

    /// Flow does not exist.
    #[error("flow_not_found: flow does not exist")]
    FlowNotFound,

    /// Execution is not a member of the specified flow.
    #[error("execution_not_in_flow: execution not in flow")]
    ExecutionNotInFlow,

    /// Dependency edge already exists.
    #[error("dependency_already_exists: edge already exists")]
    DependencyAlreadyExists,

    /// Self-referencing edge (upstream == downstream).
    #[error("self_referencing_edge: upstream and downstream are the same")]
    SelfReferencingEdge,

    /// Flow is already in a terminal state (cancelled/completed/failed).
    #[error("flow_already_terminal: flow is already terminal")]
    FlowAlreadyTerminal,

    /// Dependencies not yet satisfied (for promote_blocked_to_eligible).
    #[error("deps_not_satisfied: dependencies still unresolved")]
    DepsNotSatisfied,

    /// Not blocked by dependencies (for promote/unblock).
    #[error("not_blocked_by_deps: execution not blocked by dependencies")]
    NotBlockedByDeps,

    /// Execution not runnable (for block/unblock/promote).
    #[error("not_runnable: execution is not in runnable state")]
    NotRunnable,

    /// Execution is terminal (for block/promote).
    #[error("terminal: execution is already terminal")]
    Terminal,

    /// Invalid blocking reason for block_execution_for_admission.
    #[error("invalid_blocking_reason: unrecognized blocking reason")]
    InvalidBlockingReason,

    // ── Usage reporting ──
    /// Usage seq already processed. No-op.
    #[error("ok_already_applied: usage seq already processed")]
    OkAlreadyApplied,

    // ── Attempt errors (RFC-002) ──
    /// Attempt index doesn't exist.
    #[error("attempt_not_found: attempt index does not exist")]
    AttemptNotFound,

    /// Attempt not created. Internal sequencing error.
    #[error("attempt_not_in_created_state: internal sequencing error")]
    AttemptNotInCreatedState,

    /// Attempt not running.
    #[error("attempt_not_started: attempt not in started state")]
    AttemptNotStarted,

    /// Already ended. No-op.
    #[error("attempt_already_terminal: attempt already ended")]
    AttemptAlreadyTerminal,

    /// Execution doesn't exist.
    #[error("execution_not_found: execution does not exist")]
    ExecutionNotFound,

    /// Wrong state for new attempt.
    #[error("execution_not_eligible_for_attempt: wrong state for new attempt")]
    ExecutionNotEligibleForAttempt,

    /// Not terminal or limit reached.
    #[error("replay_not_allowed: execution not terminal or limit reached")]
    ReplayNotAllowed,

    /// Retry limit reached.
    #[error("max_retries_exhausted: retry limit reached")]
    MaxRetriesExhausted,

    // ── Stream errors (RFC-006) ──
    /// No frames appended yet. Normal for new attempts.
    #[error("stream_not_found: no frames appended yet")]
    StreamNotFound,

    /// Already closed. No-op.
    #[error("stream_already_closed: stream already closed")]
    StreamAlreadyClosed,

    /// Unrecognized frame type.
    #[error("invalid_frame_type: unrecognized frame type")]
    InvalidFrameType,

    /// Invalid Stream ID.
    #[error("invalid_offset: invalid stream ID offset")]
    InvalidOffset,

    /// Auth failed.
    #[error("unauthorized: authentication/authorization failed")]
    Unauthorized,

    // ── Budget/Quota errors (RFC-008) ──
    /// Budget doesn't exist.
    #[error("budget_not_found: budget does not exist")]
    BudgetNotFound,

    /// Malformed scope.
    #[error("invalid_budget_scope: malformed budget scope")]
    InvalidBudgetScope,

    /// Budget already attached or conflicts.
    #[error("budget_attach_conflict: budget attachment conflict")]
    BudgetAttachConflict,

    /// No operator privileges.
    #[error("budget_override_not_allowed: insufficient privileges")]
    BudgetOverrideNotAllowed,

    /// Quota doesn't exist.
    #[error("quota_policy_not_found: quota policy does not exist")]
    QuotaPolicyNotFound,

    /// Window full. Backoff retry_after_ms.
    #[error("rate_limit_exceeded: rate limit window full")]
    RateLimitExceeded,

    /// Concurrency cap hit.
    #[error("concurrency_limit_exceeded: concurrency cap reached")]
    ConcurrencyLimitExceeded,

    /// Quota already attached.
    #[error("quota_attach_conflict: quota policy already attached")]
    QuotaAttachConflict,

    /// Malformed quota definition.
    #[error("invalid_quota_spec: malformed quota policy definition")]
    InvalidQuotaSpec,

    /// Caller supplied a non-numeric value where a number is required.
    #[error("invalid_input: {0}")]
    InvalidInput(String),

    /// Worker caps do not satisfy execution's required_capabilities.
    /// Payload is the sorted-CSV of missing tokens. RETRYABLE: execution
    /// stays in the eligible ZSET for a worker with matching caps.
    #[error("capability_mismatch: missing {0}")]
    CapabilityMismatch(String),

    /// Caller supplied a malformed or oversized capability list (defense
    /// against 1MB-repeated-token payloads). TERMINAL from this call's
    /// perspective: the caller must fix its config before retrying.
    #[error("invalid_capabilities: {0}")]
    InvalidCapabilities(String),

    /// `ff_create_execution` received a `policy_json` that is not valid JSON
    /// or whose `routing_requirements` is structurally wrong (not an object,
    /// required_capabilities not an array). TERMINAL: the submitter must
    /// send a well-formed policy. Kept distinct from `invalid_capabilities`
    /// so tooling can distinguish "payload never parsed" from "payload
    /// parsed but contents rejected".
    #[error("invalid_policy_json: {0}")]
    InvalidPolicyJson(String),

    /// Pending waitpoint record is missing its HMAC token field. Returned by
    /// `ff_suspend_execution` when activating a pending waitpoint whose
    /// `waitpoint_token` field is absent or empty (pre-HMAC-upgrade record
    /// or a corrupted write). Surfacing this at activation time instead of
    /// letting every subsequent signal delivery silently reject with
    /// `missing_token` makes the degraded state visible at the right step.
    /// TERMINAL: the pending waitpoint is unrecoverable without a fresh one.
    #[error("waitpoint_not_token_bound")]
    WaitpointNotTokenBound,

    /// Rotation FCALL: `new_kid` empty or contains `:`.
    #[error("invalid_kid: kid must be non-empty and contain no ':'")]
    InvalidKid,

    /// Rotation FCALL: `new_secret_hex` empty, odd length, or non-hex.
    #[error("invalid_secret_hex: secret must be a non-empty even-length hex string")]
    InvalidSecretHex,

    /// Rotation FCALL: `grace_ms` not a non-negative integer.
    #[error("invalid_grace_ms: grace_ms must be a non-negative integer")]
    InvalidGraceMs,

    /// Rotation FCALL: same kid already installed with a different secret.
    /// Carries the kid so operators see which one conflicted. Refuse — the
    /// operator must pick a fresh kid or restore the stored secret.
    #[error("rotation_conflict: kid {0} already installed with a different secret")]
    RotationConflict(String),

    /// `ff_set_execution_tags` / `ff_set_flow_tags` received a tag key
    /// that does not match the reserved namespace `^[a-z][a-z0-9_]*\.`.
    /// Callers must prefix tag keys with `<caller>.` (e.g.
    /// `cairn.task_id`). TERMINAL: caller must fix input before retry.
    /// Payload is the offending key for precise diagnostics.
    #[error("invalid_tag_key: {0}")]
    InvalidTagKey(String),

    // ── Transport-level errors (not from Lua) ──
    /// Valkey connection or protocol error. Preserves `ferriskey::ErrorKind` so
    /// callers can distinguish transient/permanent/NOSCRIPT/MOVED/etc.
    ///
    /// Issue #171: gated behind `valkey-client` (default-on). Consumers
    /// building `ff-sdk --no-default-features` get a ferriskey-free
    /// `ScriptError` enum. All non-transport variants remain available.
    #[cfg(feature = "valkey-client")]
    #[error("valkey: {0}")]
    Valkey(#[from] ferriskey::Error),

    /// Failed to parse FCALL return value. `fcall` names the FCALL OR the
    /// nearest semantic parser (e.g. `"parse_report_usage_result"`,
    /// `"stream_tail_decode"`, `"decode_flow_snapshot"`). Never empty.
    /// `execution_id` is populated at sites where the exec_id is in scope
    /// (the 13 task.rs sites) and `None` elsewhere. `message` carries
    /// expected-vs-got detail.
    #[error("{}", fmt_parse(.fcall, .execution_id.as_deref(), .message))]
    Parse {
        fcall: String,
        execution_id: Option<String>,
        message: String,
    },
}

/// Renders `ScriptError::Parse` as
/// `parse error: <fcall>[exec=...]: <message>`. The `[exec=...]` slot is
/// omitted when `execution_id` is `None`.
fn fmt_parse(fcall: &str, execution_id: Option<&str>, message: &str) -> String {
    match execution_id {
        Some(eid) => format!("parse error: {fcall}[exec={eid}]: {message}"),
        None => format!("parse error: {fcall}: {message}"),
    }
}

impl ScriptError {
    /// Returns the underlying ferriskey ErrorKind if this is a transport error.
    ///
    /// Issue #171: gated with the `Valkey` variant behind `valkey-client`.
    #[cfg(feature = "valkey-client")]
    pub fn valkey_kind(&self) -> Option<ferriskey::ErrorKind> {
        match self {
            Self::Valkey(e) => Some(e.kind()),
            _ => None,
        }
    }

    /// Classify this error for SDK action dispatch.
    pub fn class(&self) -> ErrorClass {
        match self {
            // Terminal
            Self::StaleLease
            | Self::LeaseExpired
            | Self::LeaseRevoked
            | Self::ExecutionNotActive { .. }
            | Self::TargetNotSignalable
            | Self::PayloadTooLarge
            | Self::SignalLimitExceeded
            | Self::InvalidWaitpointKey
            | Self::ExecutionNotTerminal
            | Self::MaxReplaysExhausted
            | Self::StreamClosed
            | Self::StaleOwnerCannotAppend
            | Self::RetentionLimitExceeded
            | Self::InvalidLeaseForSuspend
            | Self::ResumeConditionNotMet
            | Self::InvalidDependency
            | Self::ExecutionAlreadyInFlow
            | Self::CycleDetected
            | Self::FlowNotFound
            | Self::ExecutionNotInFlow
            | Self::DependencyAlreadyExists
            | Self::SelfReferencingEdge
            | Self::FlowAlreadyTerminal
            | Self::InvalidWaitpointForExecution
            | Self::InvalidBlockingReason
            | Self::NotRunnable
            | Self::Terminal
            | Self::AttemptNotFound
            | Self::AttemptNotStarted
            | Self::ExecutionNotFound
            | Self::ExecutionNotEligibleForAttempt
            | Self::ReplayNotAllowed
            | Self::MaxRetriesExhausted
            | Self::Unauthorized
            | Self::BudgetNotFound
            | Self::InvalidBudgetScope
            | Self::BudgetAttachConflict
            | Self::BudgetOverrideNotAllowed
            | Self::QuotaPolicyNotFound
            | Self::QuotaAttachConflict
            | Self::InvalidQuotaSpec
            | Self::InvalidInput(_)
            | Self::InvalidCapabilities(_)
            | Self::InvalidPolicyJson(_)
            | Self::WaitpointNotTokenBound
            | Self::InvalidKid
            | Self::InvalidSecretHex
            | Self::InvalidGraceMs
            | Self::RotationConflict(_)
            | Self::InvalidTagKey(_)
            | Self::FenceRequired
            | Self::PartialFenceTriple
            | Self::Parse { .. } => ErrorClass::Terminal,

            // Transport errors classify by their ferriskey ErrorKind —
            // IoError / FatalSend / TryAgain / BusyLoading / ClusterDown are
            // genuinely retryable even though all other Valkey errors are
            // terminal from the caller's perspective.
            #[cfg(feature = "valkey-client")]
            Self::Valkey(e) => {
                if is_retryable_kind(e.kind()) {
                    ErrorClass::Retryable
                } else {
                    ErrorClass::Terminal
                }
            }

            // Retryable
            Self::UseClaimResumedExecution
            | Self::NotAResumedExecution
            | Self::ExecutionNotLeaseable
            | Self::LeaseConflict
            | Self::InvalidClaimGrant
            | Self::ClaimGrantExpired
            | Self::NoEligibleExecution
            | Self::WaitpointNotFound
            | Self::WaitpointPendingUseBufferScript
            | Self::StaleGraphRevision
            | Self::RateLimitExceeded
            | Self::ConcurrencyLimitExceeded
            | Self::CapabilityMismatch(_)
            | Self::InvalidOffset => ErrorClass::Retryable,

            // Cooperative
            Self::BudgetExceeded => ErrorClass::Cooperative,

            // Informational
            Self::ExecutionNotSuspended
            | Self::AlreadySuspended
            | Self::WaitpointClosed
            | Self::DuplicateSignal
            | Self::ExecutionNotEligible
            | Self::ExecutionNotInEligibleSet
            | Self::GrantAlreadyExists
            | Self::ExecutionNotReclaimable
            | Self::NoActiveLease
            | Self::OkAlreadyApplied
            | Self::AttemptAlreadyTerminal
            | Self::StreamAlreadyClosed
            | Self::BudgetSoftExceeded
            | Self::WaitpointAlreadyExists
            | Self::WaitpointNotOpen
            | Self::WaitpointNotPending
            | Self::PendingWaitpointExpired
            | Self::NotBlockedByDeps
            | Self::DepsNotSatisfied => ErrorClass::Informational,

            // Bug
            Self::ActiveAttemptExists | Self::AttemptNotInCreatedState => ErrorClass::Bug,

            // Expected
            Self::StreamNotFound => ErrorClass::Expected,

            // Soft error
            Self::InvalidFrameType => ErrorClass::SoftError,
        }
    }

    /// Parse an error code string (from Lua return) into a ScriptError.
    pub fn from_code(code: &str) -> Option<Self> {
        Some(match code {
            "stale_lease" => Self::StaleLease,
            "lease_expired" => Self::LeaseExpired,
            "lease_revoked" => Self::LeaseRevoked,
            "execution_not_active" => Self::ExecutionNotActive {
                terminal_outcome: String::new(),
                lease_epoch: String::new(),
                lifecycle_phase: String::new(),
                attempt_id: String::new(),
            },
            "no_active_lease" => Self::NoActiveLease,
            "active_attempt_exists" => Self::ActiveAttemptExists,
            "use_claim_resumed_execution" => Self::UseClaimResumedExecution,
            "not_a_resumed_execution" => Self::NotAResumedExecution,
            "execution_not_leaseable" => Self::ExecutionNotLeaseable,
            "lease_conflict" => Self::LeaseConflict,
            "invalid_claim_grant" => Self::InvalidClaimGrant,
            "claim_grant_expired" => Self::ClaimGrantExpired,
            "no_eligible_execution" => Self::NoEligibleExecution,
            "budget_exceeded" => Self::BudgetExceeded,
            "budget_soft_exceeded" => Self::BudgetSoftExceeded,
            "execution_not_suspended" => Self::ExecutionNotSuspended,
            "already_suspended" => Self::AlreadySuspended,
            "waitpoint_closed" => Self::WaitpointClosed,
            "waitpoint_not_found" => Self::WaitpointNotFound,
            "target_not_signalable" => Self::TargetNotSignalable,
            "waitpoint_pending_use_buffer_script" => Self::WaitpointPendingUseBufferScript,
            "duplicate_signal" => Self::DuplicateSignal,
            "payload_too_large" => Self::PayloadTooLarge,
            "signal_limit_exceeded" => Self::SignalLimitExceeded,
            "invalid_waitpoint_key" => Self::InvalidWaitpointKey,
            "invalid_lease_for_suspend" => Self::InvalidLeaseForSuspend,
            "resume_condition_not_met" => Self::ResumeConditionNotMet,
            "waitpoint_not_pending" => Self::WaitpointNotPending,
            "pending_waitpoint_expired" => Self::PendingWaitpointExpired,
            "invalid_waitpoint_for_execution" => Self::InvalidWaitpointForExecution,
            "waitpoint_already_exists" => Self::WaitpointAlreadyExists,
            "waitpoint_not_open" => Self::WaitpointNotOpen,
            "execution_not_terminal" => Self::ExecutionNotTerminal,
            "max_replays_exhausted" => Self::MaxReplaysExhausted,
            "stream_closed" => Self::StreamClosed,
            "stale_owner_cannot_append" => Self::StaleOwnerCannotAppend,
            "retention_limit_exceeded" => Self::RetentionLimitExceeded,
            "execution_not_eligible" => Self::ExecutionNotEligible,
            "execution_not_in_eligible_set" => Self::ExecutionNotInEligibleSet,
            "grant_already_exists" => Self::GrantAlreadyExists,
            "execution_not_reclaimable" => Self::ExecutionNotReclaimable,
            "invalid_dependency" => Self::InvalidDependency,
            "stale_graph_revision" => Self::StaleGraphRevision,
            "execution_already_in_flow" => Self::ExecutionAlreadyInFlow,
            "cycle_detected" => Self::CycleDetected,
            "flow_not_found" => Self::FlowNotFound,
            "execution_not_in_flow" => Self::ExecutionNotInFlow,
            "dependency_already_exists" => Self::DependencyAlreadyExists,
            "self_referencing_edge" => Self::SelfReferencingEdge,
            "flow_already_terminal" => Self::FlowAlreadyTerminal,
            "deps_not_satisfied" => Self::DepsNotSatisfied,
            "not_blocked_by_deps" => Self::NotBlockedByDeps,
            "not_runnable" => Self::NotRunnable,
            "terminal" => Self::Terminal,
            "invalid_blocking_reason" => Self::InvalidBlockingReason,
            "ok_already_applied" => Self::OkAlreadyApplied,
            "attempt_not_found" => Self::AttemptNotFound,
            "attempt_not_in_created_state" => Self::AttemptNotInCreatedState,
            "attempt_not_started" => Self::AttemptNotStarted,
            "attempt_already_terminal" => Self::AttemptAlreadyTerminal,
            "execution_not_found" => Self::ExecutionNotFound,
            "execution_not_eligible_for_attempt" => Self::ExecutionNotEligibleForAttempt,
            "replay_not_allowed" => Self::ReplayNotAllowed,
            "max_retries_exhausted" => Self::MaxRetriesExhausted,
            "stream_not_found" => Self::StreamNotFound,
            "stream_already_closed" => Self::StreamAlreadyClosed,
            "invalid_frame_type" => Self::InvalidFrameType,
            "invalid_offset" => Self::InvalidOffset,
            "unauthorized" => Self::Unauthorized,
            "budget_not_found" => Self::BudgetNotFound,
            "invalid_budget_scope" => Self::InvalidBudgetScope,
            "budget_attach_conflict" => Self::BudgetAttachConflict,
            "budget_override_not_allowed" => Self::BudgetOverrideNotAllowed,
            "quota_policy_not_found" => Self::QuotaPolicyNotFound,
            "rate_limit_exceeded" => Self::RateLimitExceeded,
            "concurrency_limit_exceeded" => Self::ConcurrencyLimitExceeded,
            "quota_attach_conflict" => Self::QuotaAttachConflict,
            "invalid_quota_spec" => Self::InvalidQuotaSpec,
            "invalid_input" => Self::InvalidInput(String::new()),
            "capability_mismatch" => Self::CapabilityMismatch(String::new()),
            "invalid_capabilities" => Self::InvalidCapabilities(String::new()),
            "invalid_policy_json" => Self::InvalidPolicyJson(String::new()),
            "waitpoint_not_token_bound" => Self::WaitpointNotTokenBound,
            "invalid_kid" => Self::InvalidKid,
            "invalid_secret_hex" => Self::InvalidSecretHex,
            "invalid_grace_ms" => Self::InvalidGraceMs,
            "rotation_conflict" => Self::RotationConflict(String::new()),
            "invalid_tag_key" => Self::InvalidTagKey(String::new()),
            "fence_required" => Self::FenceRequired,
            "partial_fence_triple" => Self::PartialFenceTriple,
            _ => return None,
        })
    }

    /// Like `from_code`, but preserves the Lua-side detail payload for
    /// variants that carry a String. Lua returns `{0, code, detail}` for
    /// capability_mismatch (missing CSV), invalid_capabilities (bounds
    /// reason), invalid_input (field name). The plain `from_code` discards
    /// the detail; callers that log or surface the detail should use this
    /// variant. Returns `None` only when the code is unknown — the detail
    /// is always folded in when applicable.
    pub fn from_code_with_detail(code: &str, detail: &str) -> Option<Self> {
        Self::from_code_with_details(code, std::slice::from_ref(&detail))
    }

    /// Like `from_code_with_detail`, but accepts multi-field details for
    /// variants whose Lua return carries more than one detail slot (e.g.
    /// `ExecutionNotActive` ships `terminal_outcome, lease_epoch,
    /// lifecycle_phase, attempt_id` at indexes 2..=5). Indexes below the
    /// variant's expected arity default to `""`.
    pub fn from_code_with_details(code: &str, details: &[&str]) -> Option<Self> {
        let base = Self::from_code(code)?;
        let d = |i: usize| details.get(i).copied().unwrap_or("").to_owned();
        Some(match base {
            Self::CapabilityMismatch(_) => Self::CapabilityMismatch(d(0)),
            Self::InvalidCapabilities(_) => Self::InvalidCapabilities(d(0)),
            Self::InvalidPolicyJson(_) => Self::InvalidPolicyJson(d(0)),
            Self::InvalidInput(_) => Self::InvalidInput(d(0)),
            Self::RotationConflict(_) => Self::RotationConflict(d(0)),
            Self::InvalidTagKey(_) => Self::InvalidTagKey(d(0)),
            Self::ExecutionNotActive { .. } => Self::ExecutionNotActive {
                terminal_outcome: d(0),
                lease_epoch: d(1),
                lifecycle_phase: d(2),
                attempt_id: d(3),
            },
            other => other,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_classification_terminal() {
        assert_eq!(ScriptError::StaleLease.class(), ErrorClass::Terminal);
        assert_eq!(ScriptError::LeaseExpired.class(), ErrorClass::Terminal);
        assert_eq!(ScriptError::ExecutionNotFound.class(), ErrorClass::Terminal);
    }

    #[test]
    fn error_classification_retryable() {
        assert_eq!(
            ScriptError::UseClaimResumedExecution.class(),
            ErrorClass::Retryable
        );
        assert_eq!(
            ScriptError::NoEligibleExecution.class(),
            ErrorClass::Retryable
        );
        assert_eq!(
            ScriptError::WaitpointNotFound.class(),
            ErrorClass::Retryable
        );
        assert_eq!(
            ScriptError::RateLimitExceeded.class(),
            ErrorClass::Retryable
        );
    }

    #[test]
    fn error_classification_cooperative() {
        assert_eq!(ScriptError::BudgetExceeded.class(), ErrorClass::Cooperative);
    }

    #[cfg(feature = "valkey-client")]
    #[test]
    fn error_classification_valkey_transient_is_retryable() {
        use ferriskey::ErrorKind;
        let transient = ScriptError::Valkey(ferriskey::Error::from((
            ErrorKind::IoError,
            "connection dropped",
        )));
        assert_eq!(transient.class(), ErrorClass::Retryable);
    }

    #[cfg(feature = "valkey-client")]
    #[test]
    fn error_classification_valkey_permanent_is_terminal() {
        use ferriskey::ErrorKind;
        let permanent = ScriptError::Valkey(ferriskey::Error::from((
            ErrorKind::AuthenticationFailed,
            "bad creds",
        )));
        assert_eq!(permanent.class(), ErrorClass::Terminal);

        // FatalReceiveError: request may have been applied, conservatively
        // terminal.
        let fatal_recv = ScriptError::Valkey(ferriskey::Error::from((
            ErrorKind::FatalReceiveError,
            "response lost",
        )));
        assert_eq!(fatal_recv.class(), ErrorClass::Terminal);
    }

    #[test]
    fn error_classification_informational() {
        assert_eq!(
            ScriptError::ExecutionNotSuspended.class(),
            ErrorClass::Informational
        );
        assert_eq!(
            ScriptError::DuplicateSignal.class(),
            ErrorClass::Informational
        );
        assert_eq!(
            ScriptError::OkAlreadyApplied.class(),
            ErrorClass::Informational
        );
    }

    #[test]
    fn error_classification_bug() {
        assert_eq!(ScriptError::ActiveAttemptExists.class(), ErrorClass::Bug);
        assert_eq!(
            ScriptError::AttemptNotInCreatedState.class(),
            ErrorClass::Bug
        );
    }

    #[test]
    fn error_classification_expected() {
        assert_eq!(ScriptError::StreamNotFound.class(), ErrorClass::Expected);
    }

    #[test]
    fn error_classification_budget_soft_exceeded() {
        // RFC-010 §10.7: budget_soft_exceeded is INFORMATIONAL
        assert_eq!(
            ScriptError::BudgetSoftExceeded.class(),
            ErrorClass::Informational
        );
    }

    #[test]
    fn error_classification_soft_error() {
        assert_eq!(ScriptError::InvalidFrameType.class(), ErrorClass::SoftError);
    }

    #[test]
    fn from_code_roundtrip() {
        let codes = [
            "stale_lease", "lease_expired", "lease_revoked",
            "execution_not_active", "no_active_lease", "active_attempt_exists",
            "use_claim_resumed_execution", "not_a_resumed_execution",
            "execution_not_leaseable", "lease_conflict",
            "invalid_claim_grant", "claim_grant_expired",
            "budget_exceeded", "budget_soft_exceeded",
            "execution_not_suspended", "already_suspended",
            "waitpoint_closed", "waitpoint_not_found",
            "target_not_signalable", "waitpoint_pending_use_buffer_script",
            "invalid_lease_for_suspend", "resume_condition_not_met",
            "signal_limit_exceeded",
            "execution_not_terminal", "max_replays_exhausted",
            "stream_closed", "stale_owner_cannot_append", "retention_limit_exceeded",
            "execution_not_eligible", "execution_not_in_eligible_set",
            "grant_already_exists", "execution_not_reclaimable",
            "invalid_dependency", "stale_graph_revision",
            "execution_already_in_flow", "cycle_detected",
            "execution_not_found", "max_retries_exhausted",
            "flow_not_found", "execution_not_in_flow",
            "dependency_already_exists", "self_referencing_edge",
            "flow_already_terminal",
            "deps_not_satisfied", "not_blocked_by_deps",
            "not_runnable", "terminal", "invalid_blocking_reason",
            "waitpoint_not_pending", "pending_waitpoint_expired",
            "invalid_waitpoint_for_execution", "waitpoint_already_exists",
            "waitpoint_not_open",
        ];
        for code in codes {
            let err = ScriptError::from_code(code);
            assert!(err.is_some(), "failed to parse code: {code}");
        }
    }

    #[test]
    fn from_code_unknown_returns_none() {
        assert!(ScriptError::from_code("nonexistent_error").is_none());
    }

    #[test]
    fn fence_required_classifies_terminal() {
        assert_eq!(ScriptError::FenceRequired.class(), ErrorClass::Terminal);
        assert_eq!(
            ScriptError::PartialFenceTriple.class(),
            ErrorClass::Terminal
        );
    }

    #[test]
    fn fence_required_from_code_roundtrips() {
        assert!(matches!(
            ScriptError::from_code("fence_required"),
            Some(ScriptError::FenceRequired)
        ));
        assert!(matches!(
            ScriptError::from_code("partial_fence_triple"),
            Some(ScriptError::PartialFenceTriple)
        ));
    }

    /// Regression (#98): `ScriptError::Parse` carries `fcall`, optional
    /// `execution_id`, and `message` separately. Display renders
    /// `parse error: <fcall>[exec=<id>]: <message>` when `execution_id`
    /// is `Some`, and omits the `[exec=...]` slot when `None`. `fcall`
    /// must never be empty.
    #[test]
    fn parse_structured_fields_render_and_match() {
        let with_exec = ScriptError::Parse {
            fcall: "ff_claim_execution".into(),
            execution_id: Some("018f-abc".into()),
            message: "expected Array".into(),
        };
        assert_eq!(
            with_exec.to_string(),
            "parse error: ff_claim_execution[exec=018f-abc]: expected Array"
        );
        assert!(matches!(
            &with_exec,
            ScriptError::Parse { execution_id: Some(e), .. } if e == "018f-abc"
        ));

        let no_exec = ScriptError::Parse {
            fcall: "stream_tail_decode".into(),
            execution_id: None,
            message: "unexpected array length 3".into(),
        };
        assert_eq!(
            no_exec.to_string(),
            "parse error: stream_tail_decode: unexpected array length 3"
        );
        assert!(matches!(
            &no_exec,
            ScriptError::Parse { execution_id: None, fcall, .. } if !fcall.is_empty()
        ));
    }
}
