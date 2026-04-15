//! Phase 1 function contracts — Args + Result types for each FCALL.
//!
//! Each Args struct defines the typed inputs to a Valkey Function.
//! Each Result enum defines the possible outcomes (success variants + error codes).

use crate::state::{AttemptType, PublicState, StateVector};
use crate::types::{
    AttemptId, AttemptIndex, ExecutionId, LaneId, LeaseEpoch, LeaseId, Namespace, SignalId,
    SuspensionId, TimestampMs, WaitpointId, WorkerId, WorkerInstanceId,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─── create_execution ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateExecutionArgs {
    pub execution_id: ExecutionId,
    pub namespace: Namespace,
    pub lane_id: LaneId,
    pub execution_kind: String,
    pub input_payload: Vec<u8>,
    #[serde(default)]
    pub payload_encoding: Option<String>,
    pub priority: i32,
    pub creator_identity: String,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub tags: HashMap<String, String>,
    /// JSON-encoded ExecutionPolicy.
    pub policy_json: String,
    /// If set and in the future, execution starts delayed.
    #[serde(default)]
    pub delay_until: Option<TimestampMs>,
    /// Partition ID (pre-computed).
    pub partition_id: u16,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CreateExecutionResult {
    /// Execution created successfully.
    Created {
        execution_id: ExecutionId,
        public_state: PublicState,
    },
    /// Idempotent duplicate — existing execution returned.
    Duplicate { execution_id: ExecutionId },
}

// ─── issue_claim_grant ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IssueClaimGrantArgs {
    pub execution_id: ExecutionId,
    pub lane_id: LaneId,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    #[serde(default)]
    pub capability_hash: Option<String>,
    #[serde(default)]
    pub route_snapshot_json: Option<String>,
    #[serde(default)]
    pub admission_summary: Option<String>,
    pub grant_ttl_ms: u64,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueClaimGrantResult {
    /// Grant issued.
    Granted { execution_id: ExecutionId },
}

// ─── claim_execution ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClaimExecutionArgs {
    pub execution_id: ExecutionId,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub lane_id: LaneId,
    pub lease_id: LeaseId,
    pub lease_ttl_ms: u64,
    pub attempt_id: AttemptId,
    /// Expected attempt index (pre-read from exec_core.total_attempt_count).
    /// Used for KEYS construction — must match what the Lua computes.
    pub expected_attempt_index: AttemptIndex,
    /// JSON-encoded attempt policy snapshot.
    #[serde(default)]
    pub attempt_policy_json: String,
    /// Per-attempt timeout in ms.
    #[serde(default)]
    pub attempt_timeout_ms: Option<u64>,
    /// Total execution deadline (absolute timestamp ms).
    #[serde(default)]
    pub execution_deadline_at: Option<i64>,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimedExecution {
    pub execution_id: ExecutionId,
    pub lease_id: LeaseId,
    pub lease_epoch: LeaseEpoch,
    pub attempt_index: AttemptIndex,
    pub attempt_id: AttemptId,
    pub attempt_type: AttemptType,
    pub lease_expires_at: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaimExecutionResult {
    /// Successfully claimed.
    Claimed(ClaimedExecution),
}

// ─── complete_execution ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompleteExecutionArgs {
    pub execution_id: ExecutionId,
    pub lease_id: LeaseId,
    pub lease_epoch: LeaseEpoch,
    pub attempt_index: AttemptIndex,
    pub attempt_id: AttemptId,
    #[serde(default)]
    pub result_payload: Option<Vec<u8>>,
    #[serde(default)]
    pub result_encoding: Option<String>,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompleteExecutionResult {
    /// Execution completed successfully.
    Completed {
        execution_id: ExecutionId,
        public_state: PublicState,
    },
}

// ─── renew_lease ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RenewLeaseArgs {
    pub execution_id: ExecutionId,
    pub attempt_index: AttemptIndex,
    pub attempt_id: AttemptId,
    pub lease_id: LeaseId,
    pub lease_epoch: LeaseEpoch,
    /// How long to extend the lease (milliseconds).
    pub lease_ttl_ms: u64,
    /// Grace period after lease_expires_at before the lease_current key is auto-deleted.
    #[serde(default = "default_lease_history_grace_ms")]
    pub lease_history_grace_ms: u64,
}

fn default_lease_history_grace_ms() -> u64 {
    60_000
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenewLeaseResult {
    /// Lease renewed.
    Renewed { expires_at: TimestampMs },
}

// ─── mark_lease_expired_if_due ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MarkLeaseExpiredArgs {
    pub execution_id: ExecutionId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarkLeaseExpiredResult {
    /// Lease was marked as expired.
    MarkedExpired,
    /// No action needed (already expired, not yet due, not active, etc.).
    AlreadySatisfied { reason: String },
}

// ─── cancel_execution ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CancelExecutionArgs {
    pub execution_id: ExecutionId,
    pub reason: String,
    /// "operator_override" bypasses lease check.
    #[serde(default)]
    pub source: Option<String>,
    /// Required if not operator_override and execution is active.
    #[serde(default)]
    pub lease_id: Option<LeaseId>,
    #[serde(default)]
    pub lease_epoch: Option<LeaseEpoch>,
    /// Required if not operator_override and execution is active.
    #[serde(default)]
    pub attempt_id: Option<AttemptId>,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CancelExecutionResult {
    /// Execution cancelled.
    Cancelled {
        execution_id: ExecutionId,
        public_state: PublicState,
    },
}

// ─── revoke_lease ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RevokeLeaseArgs {
    pub execution_id: ExecutionId,
    /// If set, only revoke if this matches the current lease. Empty string skips check.
    #[serde(default)]
    pub expected_lease_id: Option<String>,
    /// Worker instance whose lease set to clean up. Read from exec_core before calling.
    pub worker_instance_id: WorkerInstanceId,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RevokeLeaseResult {
    /// Lease revoked.
    Revoked { lease_id: String, lease_epoch: String },
    /// Already revoked or expired — no action needed.
    AlreadySatisfied { reason: String },
}

// ─── delay_execution ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DelayExecutionArgs {
    pub execution_id: ExecutionId,
    pub lease_id: LeaseId,
    pub lease_epoch: LeaseEpoch,
    pub attempt_index: AttemptIndex,
    pub attempt_id: AttemptId,
    pub delay_until: TimestampMs,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DelayExecutionResult {
    /// Execution delayed.
    Delayed {
        execution_id: ExecutionId,
        public_state: PublicState,
    },
}

// ─── move_to_waiting_children ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MoveToWaitingChildrenArgs {
    pub execution_id: ExecutionId,
    pub lease_id: LeaseId,
    pub lease_epoch: LeaseEpoch,
    pub attempt_index: AttemptIndex,
    pub attempt_id: AttemptId,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MoveToWaitingChildrenResult {
    /// Moved to waiting children.
    Moved {
        execution_id: ExecutionId,
        public_state: PublicState,
    },
}

// ─── change_priority ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangePriorityArgs {
    pub execution_id: ExecutionId,
    pub new_priority: i32,
    pub lane_id: LaneId,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangePriorityResult {
    /// Priority changed and re-scored.
    Changed { execution_id: ExecutionId },
}

// ─── update_progress ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpdateProgressArgs {
    pub execution_id: ExecutionId,
    pub lease_id: LeaseId,
    pub lease_epoch: LeaseEpoch,
    pub attempt_id: AttemptId,
    #[serde(default)]
    pub progress_pct: Option<u8>,
    #[serde(default)]
    pub progress_message: Option<String>,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateProgressResult {
    /// Progress updated.
    Updated,
}

// ═══════════════════════════════════════════════════════════════════════
// Phase 2 contracts: fail, reclaim, expire
// ═══════════════════════════════════════════════════════════════════════

// ─── fail_execution ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FailExecutionArgs {
    pub execution_id: ExecutionId,
    pub lease_id: LeaseId,
    pub lease_epoch: LeaseEpoch,
    pub attempt_index: AttemptIndex,
    pub attempt_id: AttemptId,
    pub failure_reason: String,
    pub failure_category: String,
    /// JSON-encoded retry policy (from execution policy). Empty = no retries.
    #[serde(default)]
    pub retry_policy_json: String,
    /// JSON-encoded attempt policy for the next retry attempt.
    #[serde(default)]
    pub next_attempt_policy_json: String,
}

/// Outcome of a fail_execution call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailExecutionResult {
    /// Retry was scheduled — execution is delayed with backoff.
    RetryScheduled {
        delay_until: TimestampMs,
        next_attempt_index: AttemptIndex,
    },
    /// No retries left — execution is terminal failed.
    TerminalFailed,
}

// ─── issue_reclaim_grant ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IssueReclaimGrantArgs {
    pub execution_id: ExecutionId,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub lane_id: LaneId,
    #[serde(default)]
    pub capability_hash: Option<String>,
    pub grant_ttl_ms: u64,
    #[serde(default)]
    pub route_snapshot_json: Option<String>,
    #[serde(default)]
    pub admission_summary: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueReclaimGrantResult {
    /// Reclaim grant issued.
    Granted { expires_at_ms: u64 },
}

// ─── reclaim_execution ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReclaimExecutionArgs {
    pub execution_id: ExecutionId,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub lane_id: LaneId,
    #[serde(default)]
    pub capability_hash: Option<String>,
    pub lease_id: LeaseId,
    pub lease_ttl_ms: u64,
    pub attempt_id: AttemptId,
    /// JSON-encoded attempt policy for the reclaim attempt.
    #[serde(default)]
    pub attempt_policy_json: String,
    /// Maximum reclaim count before terminal failure. Default: 100.
    #[serde(default = "default_max_reclaim_count")]
    pub max_reclaim_count: u32,
    /// Old worker instance (for old_worker_leases key construction).
    pub old_worker_instance_id: WorkerInstanceId,
    /// Current attempt index (for old_attempt/old_stream_meta key construction).
    pub current_attempt_index: AttemptIndex,
}

fn default_max_reclaim_count() -> u32 {
    100
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReclaimExecutionResult {
    /// Execution reclaimed — new attempt + new lease.
    Reclaimed {
        new_attempt_index: AttemptIndex,
        new_attempt_id: AttemptId,
        new_lease_id: LeaseId,
        new_lease_epoch: LeaseEpoch,
        lease_expires_at: TimestampMs,
    },
    /// Max reclaims exceeded — execution moved to terminal.
    MaxReclaimsExceeded,
}

// ─── expire_execution ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExpireExecutionArgs {
    pub execution_id: ExecutionId,
    /// "attempt_timeout" or "execution_deadline"
    pub expire_reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExpireExecutionResult {
    /// Execution expired.
    Expired { execution_id: ExecutionId },
    /// Already terminal — no-op.
    AlreadyTerminal,
}

// ═══════════════════════════════════════════════════════════════════════
// Phase 3 contracts: suspend, signal, resume, waitpoint
// ═══════════════════════════════════════════════════════════════════════

// ─── suspend_execution ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SuspendExecutionArgs {
    pub execution_id: ExecutionId,
    pub lease_id: LeaseId,
    pub lease_epoch: LeaseEpoch,
    pub attempt_index: AttemptIndex,
    pub attempt_id: AttemptId,
    pub suspension_id: SuspensionId,
    pub waitpoint_id: WaitpointId,
    pub waitpoint_key: String,
    pub reason_code: String,
    pub requested_by: String,
    pub resume_condition_json: String,
    pub resume_policy_json: String,
    #[serde(default)]
    pub continuation_metadata_pointer: Option<String>,
    #[serde(default)]
    pub timeout_at: Option<TimestampMs>,
    /// true to activate a pending waitpoint, false to create new.
    #[serde(default)]
    pub use_pending_waitpoint: bool,
    /// Timeout behavior: "fail", "cancel", "expire", "auto_resume", "escalate".
    #[serde(default = "default_timeout_behavior")]
    pub timeout_behavior: String,
}

fn default_timeout_behavior() -> String {
    "fail".to_owned()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SuspendExecutionResult {
    /// Execution suspended, waitpoint active.
    Suspended {
        suspension_id: SuspensionId,
        waitpoint_id: WaitpointId,
        waitpoint_key: String,
    },
    /// Buffered signals already satisfied the condition — suspension skipped.
    /// Lease is still held.
    AlreadySatisfied {
        suspension_id: SuspensionId,
        waitpoint_id: WaitpointId,
        waitpoint_key: String,
    },
}

// ─── resume_execution ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResumeExecutionArgs {
    pub execution_id: ExecutionId,
    /// "signal", "operator", "auto_resume"
    #[serde(default = "default_trigger_type")]
    pub trigger_type: String,
    /// Optional delay before becoming eligible (ms).
    #[serde(default)]
    pub resume_delay_ms: u64,
}

fn default_trigger_type() -> String {
    "signal".to_owned()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResumeExecutionResult {
    /// Execution resumed to runnable.
    Resumed { public_state: PublicState },
}

// ─── create_pending_waitpoint ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreatePendingWaitpointArgs {
    pub execution_id: ExecutionId,
    pub lease_id: LeaseId,
    pub lease_epoch: LeaseEpoch,
    pub attempt_index: AttemptIndex,
    pub attempt_id: AttemptId,
    pub waitpoint_id: WaitpointId,
    pub waitpoint_key: String,
    /// Short expiry for the pending waitpoint (ms).
    pub expires_in_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CreatePendingWaitpointResult {
    /// Pending waitpoint created.
    Created {
        waitpoint_id: WaitpointId,
        waitpoint_key: String,
    },
}

// ─── close_waitpoint ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CloseWaitpointArgs {
    pub waitpoint_id: WaitpointId,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CloseWaitpointResult {
    /// Waitpoint closed.
    Closed,
}

// ─── deliver_signal ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeliverSignalArgs {
    pub execution_id: ExecutionId,
    pub waitpoint_id: WaitpointId,
    pub signal_id: SignalId,
    pub signal_name: String,
    pub signal_category: String,
    pub source_type: String,
    pub source_identity: String,
    #[serde(default)]
    pub payload: Option<Vec<u8>>,
    #[serde(default)]
    pub payload_encoding: Option<String>,
    #[serde(default)]
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    pub target_scope: String,
    #[serde(default)]
    pub created_at: Option<TimestampMs>,
    /// Dedup TTL for idempotency key (ms).
    #[serde(default)]
    pub dedup_ttl_ms: Option<u64>,
    /// Resume delay after signal satisfaction (ms).
    #[serde(default)]
    pub resume_delay_ms: Option<u64>,
    /// Max signals per execution (default 10000).
    #[serde(default)]
    pub max_signals_per_execution: Option<u64>,
    /// MAXLEN for the waitpoint signal stream.
    #[serde(default)]
    pub signal_maxlen: Option<u64>,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliverSignalResult {
    /// Signal accepted with the given effect.
    Accepted {
        signal_id: SignalId,
        effect: String,
    },
    /// Duplicate signal (idempotency key matched).
    Duplicate { existing_signal_id: SignalId },
}

// ─── buffer_signal_for_pending_waitpoint ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BufferSignalArgs {
    pub execution_id: ExecutionId,
    pub waitpoint_id: WaitpointId,
    pub signal_id: SignalId,
    pub signal_name: String,
    pub signal_category: String,
    pub source_type: String,
    pub source_identity: String,
    #[serde(default)]
    pub payload: Option<Vec<u8>>,
    #[serde(default)]
    pub payload_encoding: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    pub target_scope: String,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BufferSignalResult {
    /// Signal buffered for pending waitpoint.
    Buffered { signal_id: SignalId },
    /// Duplicate signal.
    Duplicate { existing_signal_id: SignalId },
}

// ─── expire_suspension ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExpireSuspensionArgs {
    pub execution_id: ExecutionId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExpireSuspensionResult {
    /// Suspension expired with the given behavior applied.
    Expired { behavior_applied: String },
    /// Already resolved — no action needed.
    AlreadySatisfied { reason: String },
}

// ─── claim_resumed_execution ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClaimResumedExecutionArgs {
    pub execution_id: ExecutionId,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub lane_id: LaneId,
    pub lease_id: LeaseId,
    pub lease_ttl_ms: u64,
    /// Current attempt index (for KEYS construction — from exec_core).
    pub current_attempt_index: AttemptIndex,
    /// Remaining attempt timeout from before suspension (ms). 0 = no timeout.
    #[serde(default)]
    pub remaining_attempt_timeout_ms: Option<u64>,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimedResumedExecution {
    pub execution_id: ExecutionId,
    pub lease_id: LeaseId,
    pub lease_epoch: LeaseEpoch,
    pub attempt_index: AttemptIndex,
    pub attempt_id: AttemptId,
    pub lease_expires_at: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaimResumedExecutionResult {
    /// Successfully claimed resumed execution (same attempt continues).
    Claimed(ClaimedResumedExecution),
}

// ═══════════════════════════════════════════════════════════════════════
// Phase 4 contracts: stream
// ═══════════════════════════════════════════════════════════════════════

// ─── append_frame ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppendFrameArgs {
    pub execution_id: ExecutionId,
    pub attempt_index: AttemptIndex,
    pub lease_id: LeaseId,
    pub lease_epoch: LeaseEpoch,
    pub attempt_id: AttemptId,
    pub frame_type: String,
    pub timestamp: TimestampMs,
    pub payload: Vec<u8>,
    #[serde(default)]
    pub encoding: Option<String>,
    /// Optional structured metadata for the frame (JSON blob).
    #[serde(default)]
    pub metadata_json: Option<String>,
    #[serde(default)]
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    /// MAXLEN for the stream. 0 = no trim.
    #[serde(default)]
    pub retention_maxlen: Option<u32>,
    /// Max payload bytes per frame. Default: 65536.
    #[serde(default)]
    pub max_payload_bytes: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppendFrameResult {
    /// Frame appended successfully.
    Appended {
        /// Valkey Stream entry ID (e.g. "1713100800150-0").
        entry_id: String,
        /// Total frame count after this append.
        frame_count: u64,
    },
}

// ═══════════════════════════════════════════════════════════════════════
// Phase 5 contracts: budget, quota, block/unblock
// ═══════════════════════════════════════════════════════════════════════

// ─── report_usage_and_check ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReportUsageArgs {
    /// Dimension names to increment.
    pub dimensions: Vec<String>,
    /// Increment values (parallel with dimensions).
    pub deltas: Vec<u64>,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportUsageResult {
    /// All increments applied, no breach.
    Ok,
    /// Soft limit breached on a dimension (advisory, increments applied).
    SoftBreach {
        dimension: String,
        action: String,
    },
    /// Hard limit breached (increments NOT applied).
    HardBreach {
        dimension: String,
        action: String,
        current_usage: u64,
        hard_limit: u64,
    },
}

// ─── reset_budget ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResetBudgetArgs {
    pub budget_id: crate::types::BudgetId,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResetBudgetResult {
    /// Budget reset successfully.
    Reset { next_reset_at: TimestampMs },
}

// ─── check_admission_and_record ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckAdmissionArgs {
    pub execution_id: ExecutionId,
    pub now: TimestampMs,
    pub window_seconds: u64,
    pub rate_limit: u64,
    pub concurrency_cap: u64,
    #[serde(default)]
    pub jitter_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckAdmissionResult {
    /// Admitted — execution may proceed.
    Admitted,
    /// Already admitted in this window (idempotent).
    AlreadyAdmitted,
    /// Rate limit exceeded.
    RateExceeded { retry_after_ms: u64 },
    /// Concurrency cap hit.
    ConcurrencyExceeded,
}

// ─── block_execution_for_admission ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockExecutionArgs {
    pub execution_id: ExecutionId,
    pub blocking_reason: String,
    #[serde(default)]
    pub blocking_detail: Option<String>,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockExecutionResult {
    /// Execution blocked.
    Blocked,
}

// ─── unblock_execution ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UnblockExecutionArgs {
    pub execution_id: ExecutionId,
    pub now: TimestampMs,
    /// Expected blocking reason (prevents stale unblock).
    #[serde(default)]
    pub expected_blocking_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnblockExecutionResult {
    /// Execution unblocked and moved to eligible.
    Unblocked,
}

// ═══════════════════════════════════════════════════════════════════════
// Phase 6 contracts: flow coordination and dependencies
// ═══════════════════════════════════════════════════════════════════════

// ─── stage_dependency_edge ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StageDependencyEdgeArgs {
    pub flow_id: crate::types::FlowId,
    pub edge_id: crate::types::EdgeId,
    pub upstream_execution_id: ExecutionId,
    pub downstream_execution_id: ExecutionId,
    #[serde(default = "default_dependency_kind")]
    pub dependency_kind: String,
    #[serde(default)]
    pub data_passing_ref: Option<String>,
    pub expected_graph_revision: u64,
    pub now: TimestampMs,
}

fn default_dependency_kind() -> String {
    "success_only".to_owned()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StageDependencyEdgeResult {
    /// Edge staged, new graph revision.
    Staged {
        edge_id: crate::types::EdgeId,
        new_graph_revision: u64,
    },
}

// ─── apply_dependency_to_child ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApplyDependencyToChildArgs {
    pub flow_id: crate::types::FlowId,
    pub edge_id: crate::types::EdgeId,
    pub upstream_execution_id: ExecutionId,
    pub graph_revision: u64,
    #[serde(default = "default_dependency_kind")]
    pub dependency_kind: String,
    #[serde(default)]
    pub data_passing_ref: Option<String>,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApplyDependencyToChildResult {
    /// Dependency applied, N unsatisfied deps remaining.
    Applied { unsatisfied_count: u32 },
    /// Already applied (idempotent).
    AlreadyApplied,
}

// ─── resolve_dependency ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResolveDependencyArgs {
    pub edge_id: crate::types::EdgeId,
    /// "success", "failed", "cancelled", "expired", "skipped"
    pub upstream_outcome: String,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolveDependencyResult {
    /// Edge satisfied — downstream may become eligible.
    Satisfied,
    /// Edge made impossible — downstream becomes skipped.
    Impossible,
    /// Already resolved (idempotent).
    AlreadyResolved,
}

// ─── promote_blocked_to_eligible ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromoteBlockedToEligibleArgs {
    pub execution_id: ExecutionId,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromoteBlockedToEligibleResult {
    Promoted,
}

// ─── evaluate_flow_eligibility ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluateFlowEligibilityArgs {
    pub execution_id: ExecutionId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvaluateFlowEligibilityResult {
    /// Execution eligibility status.
    Status { status: String },
}

// ─── replay_execution ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayExecutionArgs {
    pub execution_id: ExecutionId,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplayExecutionResult {
    /// Replayed to runnable.
    Replayed { public_state: PublicState },
}

// ─── Common sub-types ───

/// Summary of state after a mutation, returned by many functions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSummary {
    pub state_vector: StateVector,
    pub current_attempt_index: AttemptIndex,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_execution_args_serde() {
        let args = CreateExecutionArgs {
            execution_id: ExecutionId::new(),
            namespace: Namespace::new("test"),
            lane_id: LaneId::new("default"),
            execution_kind: "llm_call".to_owned(),
            input_payload: b"hello".to_vec(),
            payload_encoding: Some("json".to_owned()),
            priority: 0,
            creator_identity: "test-user".to_owned(),
            idempotency_key: None,
            tags: HashMap::new(),
            policy_json: "{}".to_owned(),
            delay_until: None,
            partition_id: 42,
            now: TimestampMs::now(),
        };
        let json = serde_json::to_string(&args).unwrap();
        let parsed: CreateExecutionArgs = serde_json::from_str(&json).unwrap();
        assert_eq!(args.execution_id, parsed.execution_id);
    }

    #[test]
    fn claim_result_serde() {
        let result = ClaimExecutionResult::Claimed(ClaimedExecution {
            execution_id: ExecutionId::new(),
            lease_id: LeaseId::new(),
            lease_epoch: LeaseEpoch::new(1),
            attempt_index: AttemptIndex::new(0),
            attempt_id: AttemptId::new(),
            attempt_type: AttemptType::Initial,
            lease_expires_at: TimestampMs::from_millis(1000),
        });
        let json = serde_json::to_string(&result).unwrap();
        let parsed: ClaimExecutionResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, parsed);
    }
}
