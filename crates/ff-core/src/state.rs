use serde::{Deserialize, Serialize};

// ── Dimension A — Lifecycle Phase ──

/// What major phase of existence is this execution in?
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecyclePhase {
    /// Accepted by engine, not yet resolved to runnable/delayed. Transient.
    Submitted,
    /// Eligible or potentially eligible for claiming.
    Runnable,
    /// Currently owned by a worker lease and in progress.
    Active,
    /// Intentionally paused, waiting for signal/approval/callback.
    Suspended,
    /// Execution is finished. No further state transitions except replay.
    Terminal,
}

// ── Dimension B — Ownership State ──

/// Who, if anyone, is currently allowed to mutate active execution state?
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnershipState {
    /// No current lease.
    Unowned,
    /// A worker holds a valid lease.
    Leased,
    /// Lease TTL passed without renewal. Execution awaits reclaim.
    LeaseExpiredReclaimable,
    /// Lease was explicitly revoked by operator or engine.
    LeaseRevoked,
}

// ── Dimension C — Eligibility State ──

/// Can the execution be claimed for work right now?
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EligibilityState {
    /// Ready for claiming.
    EligibleNow,
    /// Delayed until a future timestamp.
    NotEligibleUntilTime,
    /// Waiting on upstream executions in a flow/DAG.
    BlockedByDependencies,
    /// Budget limit reached.
    BlockedByBudget,
    /// Quota or rate-limit reached.
    BlockedByQuota,
    /// No capable/available worker matches requirements.
    BlockedByRoute,
    /// Lane is paused or draining.
    BlockedByLaneState,
    /// Operator hold.
    BlockedByOperator,
    /// Used when lifecycle_phase is active, suspended, or terminal.
    NotApplicable,
}

// ── Dimension D — Blocking Reason ──

/// What is the most specific explanation for lack of forward progress?
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockingReason {
    /// Not blocked.
    None,
    /// Eligible but no worker has claimed yet.
    WaitingForWorker,
    /// Delayed for retry backoff.
    WaitingForRetryBackoff,
    /// Delayed after suspension resume.
    WaitingForResumeDelay,
    /// Worker-initiated explicit delay.
    WaitingForDelay,
    /// Suspended, waiting for a generic signal.
    WaitingForSignal,
    /// Suspended, waiting for human approval.
    WaitingForApproval,
    /// Suspended, waiting for external callback.
    WaitingForCallback,
    /// Suspended, waiting for tool completion.
    WaitingForToolResult,
    /// Blocked on child/dependency executions.
    WaitingForChildren,
    /// Budget exhausted.
    WaitingForBudget,
    /// Quota/rate-limit window full.
    WaitingForQuota,
    /// No worker with required capabilities available.
    WaitingForCapableWorker,
    /// No worker in required region/locality.
    WaitingForLocalityMatch,
    /// Operator placed a hold.
    PausedByOperator,
    /// Policy rule prevents progress (e.g. lane pause).
    PausedByPolicy,
    /// Flow cancellation with let_active_finish blocked this unclaimed member.
    PausedByFlowCancel,
}

// ── Dimension E — Terminal Outcome ──

/// If the execution is terminal, how did it end?
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalOutcome {
    /// Not terminal.
    None,
    /// Completed successfully with result.
    Success,
    /// Failed after exhausting retries or by explicit failure.
    Failed,
    /// Intentionally terminated by user, operator, or policy.
    Cancelled,
    /// Deadline, TTL, or suspension timeout elapsed.
    Expired,
    /// Required dependency failed, making this execution impossible.
    Skipped,
}

// ── Dimension F — Attempt State ──

/// What is happening at the concrete run-attempt layer?
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    /// No attempt context (e.g. freshly submitted, or skipped before any attempt).
    None,
    /// Awaiting initial claim.
    PendingFirstAttempt,
    /// An attempt is actively executing.
    RunningAttempt,
    /// Current attempt was interrupted (crash, reclaim, suspension, delay, waiting children).
    AttemptInterrupted,
    /// Awaiting retry after failure.
    PendingRetryAttempt,
    /// Awaiting replay after terminal state.
    PendingReplayAttempt,
    /// The final attempt has concluded.
    AttemptTerminal,
}

// ── Derived: Public State ──

/// Engine-computed user-facing label. Derived from the state vector.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicState {
    /// Eligible and waiting for a worker to claim.
    Waiting,
    /// Not yet eligible due to time-based delay.
    Delayed,
    /// Blocked by budget, quota, or rate-limit policy.
    RateLimited,
    /// Blocked on child/dependency executions.
    WaitingChildren,
    /// Currently being processed by a worker.
    Active,
    /// Intentionally paused, waiting for signal/approval/callback.
    Suspended,
    /// Terminal: finished successfully.
    Completed,
    /// Terminal: finished unsuccessfully.
    Failed,
    /// Terminal: intentionally terminated.
    Cancelled,
    /// Terminal: deadline/TTL elapsed.
    Expired,
    /// Terminal: impossible to run because required dependency failed.
    Skipped,
}

// ── State Vector ──

/// The full 6+1 dimension execution state vector.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateVector {
    pub lifecycle_phase: LifecyclePhase,
    pub ownership_state: OwnershipState,
    pub eligibility_state: EligibilityState,
    pub blocking_reason: BlockingReason,
    pub terminal_outcome: TerminalOutcome,
    pub attempt_state: AttemptState,
    /// Engine-derived user-facing label.
    pub public_state: PublicState,
}

impl StateVector {
    /// Derive public_state from the other 6 dimensions per RFC-001 §2.4.
    ///
    /// Never panics. In distributed systems, constraint violations can occur
    /// via partial writes or reconciler drift. Impossible combinations log a
    /// warning and return a safe fallback instead of crashing.
    pub fn derive_public_state(&self) -> PublicState {
        match self.lifecycle_phase {
            LifecyclePhase::Terminal => match self.terminal_outcome {
                TerminalOutcome::Success => PublicState::Completed,
                TerminalOutcome::Failed => PublicState::Failed,
                TerminalOutcome::Cancelled => PublicState::Cancelled,
                TerminalOutcome::Expired => PublicState::Expired,
                TerminalOutcome::Skipped => PublicState::Skipped,
                TerminalOutcome::None => {
                    // V4 violation: terminal without outcome. Corrupt state —
                    // surface as Failed so operators notice and investigate.
                    // No logging here (ff-core has no tracing dep); callers
                    // detect this via is_consistent() returning false.
                    PublicState::Failed
                }
            },
            LifecyclePhase::Suspended => PublicState::Suspended,
            LifecyclePhase::Active => PublicState::Active,
            LifecyclePhase::Runnable => match self.eligibility_state {
                EligibilityState::EligibleNow => PublicState::Waiting,
                EligibilityState::NotEligibleUntilTime => PublicState::Delayed,
                EligibilityState::BlockedByDependencies => PublicState::WaitingChildren,
                EligibilityState::BlockedByBudget | EligibilityState::BlockedByQuota => {
                    PublicState::RateLimited
                }
                EligibilityState::BlockedByRoute
                | EligibilityState::BlockedByLaneState
                | EligibilityState::BlockedByOperator => PublicState::Waiting,
                EligibilityState::NotApplicable => {
                    // Constraint violation: runnable should not have not_applicable.
                    // Surface as Waiting — the index reconciler will correct this.
                    PublicState::Waiting
                }
            },
            LifecyclePhase::Submitted => PublicState::Waiting,
        }
    }

    /// Check if the stored public_state matches the derived value.
    pub fn is_consistent(&self) -> bool {
        self.public_state == self.derive_public_state()
    }
}

// ── Attempt Lifecycle (RFC-002) ──

/// Per-attempt lifecycle states.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptLifecycle {
    /// Attempt record exists; worker has not yet acquired a lease.
    Created,
    /// Worker has acquired a lease and is actively executing.
    Started,
    /// Attempt is paused because the execution intentionally suspended.
    Suspended,
    /// Attempt completed successfully.
    EndedSuccess,
    /// Attempt failed.
    EndedFailure,
    /// Attempt was cancelled.
    EndedCancelled,
    /// Attempt was interrupted by lease expiry/revocation and the execution was reclaimed.
    InterruptedReclaimed,
}

impl AttemptLifecycle {
    /// Whether this attempt lifecycle state is terminal.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::EndedSuccess
                | Self::EndedFailure
                | Self::EndedCancelled
                | Self::InterruptedReclaimed
        )
    }
}

// ── Attempt Type (RFC-002) ──

/// Why this attempt was created.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptType {
    /// First attempt for this execution.
    Initial,
    /// Retry after a failed attempt.
    Retry,
    /// Reclaim after lease expiry or revocation.
    Reclaim,
    /// Replay of a terminal execution.
    Replay,
    /// Fallback progression to next provider/model.
    Fallback,
}

// ── Lane State (RFC-009) ──

/// Lane operational state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneState {
    /// Lane is accepting and processing work.
    Active,
    /// Lane is accepting submissions but not claiming (paused).
    Paused,
    /// Lane is not accepting new submissions but processing existing.
    Draining,
    /// Lane is disabled.
    Disabled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_public_state_terminal_success() {
        let sv = StateVector {
            lifecycle_phase: LifecyclePhase::Terminal,
            ownership_state: OwnershipState::Unowned,
            eligibility_state: EligibilityState::NotApplicable,
            blocking_reason: BlockingReason::None,
            terminal_outcome: TerminalOutcome::Success,
            attempt_state: AttemptState::AttemptTerminal,
            public_state: PublicState::Completed,
        };
        assert_eq!(sv.derive_public_state(), PublicState::Completed);
        assert!(sv.is_consistent());
    }

    #[test]
    fn derive_public_state_active() {
        let sv = StateVector {
            lifecycle_phase: LifecyclePhase::Active,
            ownership_state: OwnershipState::Leased,
            eligibility_state: EligibilityState::NotApplicable,
            blocking_reason: BlockingReason::None,
            terminal_outcome: TerminalOutcome::None,
            attempt_state: AttemptState::RunningAttempt,
            public_state: PublicState::Active,
        };
        assert_eq!(sv.derive_public_state(), PublicState::Active);
        assert!(sv.is_consistent());
    }

    #[test]
    fn derive_public_state_active_lease_expired_still_active() {
        // D3: Active dominates ownership nuance
        let sv = StateVector {
            lifecycle_phase: LifecyclePhase::Active,
            ownership_state: OwnershipState::LeaseExpiredReclaimable,
            eligibility_state: EligibilityState::NotApplicable,
            blocking_reason: BlockingReason::None,
            terminal_outcome: TerminalOutcome::None,
            attempt_state: AttemptState::AttemptInterrupted,
            public_state: PublicState::Active,
        };
        assert_eq!(sv.derive_public_state(), PublicState::Active);
    }

    #[test]
    fn derive_public_state_runnable_eligible() {
        let sv = StateVector {
            lifecycle_phase: LifecyclePhase::Runnable,
            ownership_state: OwnershipState::Unowned,
            eligibility_state: EligibilityState::EligibleNow,
            blocking_reason: BlockingReason::WaitingForWorker,
            terminal_outcome: TerminalOutcome::None,
            attempt_state: AttemptState::PendingFirstAttempt,
            public_state: PublicState::Waiting,
        };
        assert_eq!(sv.derive_public_state(), PublicState::Waiting);
    }

    #[test]
    fn derive_public_state_delayed() {
        let sv = StateVector {
            lifecycle_phase: LifecyclePhase::Runnable,
            ownership_state: OwnershipState::Unowned,
            eligibility_state: EligibilityState::NotEligibleUntilTime,
            blocking_reason: BlockingReason::WaitingForRetryBackoff,
            terminal_outcome: TerminalOutcome::None,
            attempt_state: AttemptState::PendingRetryAttempt,
            public_state: PublicState::Delayed,
        };
        assert_eq!(sv.derive_public_state(), PublicState::Delayed);
    }

    #[test]
    fn derive_public_state_waiting_children() {
        let sv = StateVector {
            lifecycle_phase: LifecyclePhase::Runnable,
            ownership_state: OwnershipState::Unowned,
            eligibility_state: EligibilityState::BlockedByDependencies,
            blocking_reason: BlockingReason::WaitingForChildren,
            terminal_outcome: TerminalOutcome::None,
            attempt_state: AttemptState::PendingFirstAttempt,
            public_state: PublicState::WaitingChildren,
        };
        assert_eq!(sv.derive_public_state(), PublicState::WaitingChildren);
    }

    #[test]
    fn derive_public_state_rate_limited() {
        let sv = StateVector {
            lifecycle_phase: LifecyclePhase::Runnable,
            ownership_state: OwnershipState::Unowned,
            eligibility_state: EligibilityState::BlockedByBudget,
            blocking_reason: BlockingReason::WaitingForBudget,
            terminal_outcome: TerminalOutcome::None,
            attempt_state: AttemptState::PendingFirstAttempt,
            public_state: PublicState::RateLimited,
        };
        assert_eq!(sv.derive_public_state(), PublicState::RateLimited);
    }

    #[test]
    fn derive_public_state_suspended() {
        let sv = StateVector {
            lifecycle_phase: LifecyclePhase::Suspended,
            ownership_state: OwnershipState::Unowned,
            eligibility_state: EligibilityState::NotApplicable,
            blocking_reason: BlockingReason::WaitingForApproval,
            terminal_outcome: TerminalOutcome::None,
            attempt_state: AttemptState::AttemptInterrupted,
            public_state: PublicState::Suspended,
        };
        assert_eq!(sv.derive_public_state(), PublicState::Suspended);
    }

    #[test]
    fn derive_public_state_submitted_collapses_to_waiting() {
        let sv = StateVector {
            lifecycle_phase: LifecyclePhase::Submitted,
            ownership_state: OwnershipState::Unowned,
            eligibility_state: EligibilityState::NotApplicable,
            blocking_reason: BlockingReason::None,
            terminal_outcome: TerminalOutcome::None,
            attempt_state: AttemptState::None,
            public_state: PublicState::Waiting,
        };
        assert_eq!(sv.derive_public_state(), PublicState::Waiting);
    }

    #[test]
    fn derive_public_state_skipped() {
        let sv = StateVector {
            lifecycle_phase: LifecyclePhase::Terminal,
            ownership_state: OwnershipState::Unowned,
            eligibility_state: EligibilityState::NotApplicable,
            blocking_reason: BlockingReason::None,
            terminal_outcome: TerminalOutcome::Skipped,
            attempt_state: AttemptState::None,
            public_state: PublicState::Skipped,
        };
        assert_eq!(sv.derive_public_state(), PublicState::Skipped);
    }

    #[test]
    fn attempt_lifecycle_terminal_check() {
        assert!(AttemptLifecycle::EndedSuccess.is_terminal());
        assert!(AttemptLifecycle::EndedFailure.is_terminal());
        assert!(AttemptLifecycle::EndedCancelled.is_terminal());
        assert!(AttemptLifecycle::InterruptedReclaimed.is_terminal());
        assert!(!AttemptLifecycle::Created.is_terminal());
        assert!(!AttemptLifecycle::Started.is_terminal());
        assert!(!AttemptLifecycle::Suspended.is_terminal());
    }

    #[test]
    fn serde_roundtrip_lifecycle_phase() {
        let phase = LifecyclePhase::Active;
        let json = serde_json::to_string(&phase).unwrap();
        assert_eq!(json, "\"active\"");
        let parsed: LifecyclePhase = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, phase);
    }

    #[test]
    fn serde_roundtrip_blocking_reason() {
        let reason = BlockingReason::PausedByFlowCancel;
        let json = serde_json::to_string(&reason).unwrap();
        assert_eq!(json, "\"paused_by_flow_cancel\"");
        let parsed: BlockingReason = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, reason);
    }

    #[test]
    fn serde_roundtrip_state_vector() {
        let sv = StateVector {
            lifecycle_phase: LifecyclePhase::Active,
            ownership_state: OwnershipState::Leased,
            eligibility_state: EligibilityState::NotApplicable,
            blocking_reason: BlockingReason::None,
            terminal_outcome: TerminalOutcome::None,
            attempt_state: AttemptState::RunningAttempt,
            public_state: PublicState::Active,
        };
        let json = serde_json::to_string(&sv).unwrap();
        let parsed: StateVector = serde_json::from_str(&json).unwrap();
        assert_eq!(sv, parsed);
    }
}
