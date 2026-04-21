//! Phase 1 function contracts — Args + Result types for each FCALL.
//!
//! Each Args struct defines the typed inputs to a Valkey Function.
//! Each Result enum defines the possible outcomes (success variants + error codes).

use crate::policy::ExecutionPolicy;
use crate::state::{AttemptType, PublicState, StateVector};
use crate::types::{
    AttemptId, AttemptIndex, CancelSource, ExecutionId, FlowId, LaneId, LeaseEpoch, LeaseId,
    Namespace, SignalId, SuspensionId, TimestampMs, WaitpointId, WaitpointToken, WorkerId,
    WorkerInstanceId,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

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
    /// Execution policy (retry, timeout, suspension, routing, etc.).
    #[serde(default)]
    pub policy: Option<ExecutionPolicy>,
    /// If set and in the future, execution starts delayed.
    #[serde(default)]
    pub delay_until: Option<TimestampMs>,
    /// Absolute deadline timestamp (ms). Execution expires if not complete by this time.
    #[serde(default)]
    pub execution_deadline_at: Option<TimestampMs>,
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
    /// Capabilities this worker advertises. Serialized as a sorted,
    /// comma-separated string to the Lua FCALL (see scheduling.lua
    /// ff_issue_claim_grant). An empty set matches only executions whose
    /// `required_capabilities` is also empty.
    #[serde(default)]
    pub worker_capabilities: BTreeSet<String>,
    pub grant_ttl_ms: u64,
    /// Caller-side timestamp for bookkeeping. NOT passed to the Lua FCALL —
    /// ff_issue_claim_grant uses `redis.call("TIME")` for grant_expires_at.
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueClaimGrantResult {
    /// Grant issued.
    Granted { execution_id: ExecutionId },
}

/// A claim grant issued by the scheduler for a specific execution.
///
/// The worker uses this to call `ff_claim_execution` (or
/// `ff_acquire_lease`), which atomically consumes the grant and
/// creates the lease.
///
/// Shared wire-level type between `ff-scheduler` (issuer) and
/// `ff-sdk` (consumer, via `FlowFabricWorker::claim_from_grant`).
/// Lives in `ff-core` so neither crate needs a dep on the other.
///
/// **Lane asymmetry with [`ReclaimGrant`]:** `ClaimGrant` does NOT
/// carry `lane_id`. The issuing scheduler's caller already picked
/// a lane (that's how admission reached this grant) and passes it
/// through to `claim_from_grant` as a separate argument. The grant
/// handle stays narrow to what uniquely identifies the admission
/// decision. The matching field on [`ReclaimGrant`] is an
/// intentional divergence — see the note on that type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimGrant {
    /// The execution that was granted.
    pub execution_id: ExecutionId,
    /// The partition where this execution lives.
    pub partition: crate::partition::Partition,
    /// The Valkey key holding the grant hash (for the worker to
    /// reference).
    pub grant_key: String,
    /// When the grant expires if not consumed.
    pub expires_at_ms: u64,
}

/// A reclaim grant issued for a resumed (attempt_interrupted) execution.
///
/// Issued by a producer (typically `ff-scheduler` once a Batch-C
/// reclaim scanner is in place; test fixtures in the interim — no
/// production Rust caller exists in-tree today). Consumed by
/// [`FlowFabricWorker::claim_from_reclaim_grant`], which calls
/// `ff_claim_resumed_execution` atomically: that FCALL validates the
/// grant, consumes it, and transitions `attempt_interrupted` →
/// `started` while preserving the existing `attempt_index` +
/// `attempt_id` (a resumed execution re-uses its attempt; it does
/// not start a new one).
///
/// Mirrors [`ClaimGrant`] for the resume path. Differences:
///
///   * [`ClaimGrant`] is issued against a freshly-eligible
///     execution and `ff_claim_execution` creates a new attempt.
///   * [`ReclaimGrant`] is issued against an `attempt_interrupted`
///     execution; `ff_claim_resumed_execution` re-uses the existing
///     attempt and bumps the lease epoch.
///
/// The grant itself is written to the same `claim_grant` Valkey key
/// that [`ClaimGrant`] uses; the distinction is which Lua FCALL
/// consumes it (`ff_claim_execution` for new attempts,
/// `ff_claim_resumed_execution` for resumes).
///
/// **Lane asymmetry with [`ClaimGrant`]:** `ReclaimGrant` CARRIES
/// `lane_id` as a field. The issuing path already knows the lane
/// (it's read from `exec_core` at grant time); carrying it here
/// spares the consumer a `HGET exec_core lane_id` round trip on
/// the hot claim path. The asymmetry is intentional — prefer
/// one-fewer-HGET on a type that already lives with the resumer's
/// lifecycle over strict handle symmetry with `ClaimGrant`.
///
/// Shared wire-level type between the eventual `ff-scheduler`
/// producer (Batch-C reclaim scanner — not yet in-tree; test
/// fixtures construct this type today) and `ff-sdk` (consumer, via
/// `FlowFabricWorker::claim_from_reclaim_grant`). Lives in
/// `ff-core` so neither crate needs a dep on the other.
///
/// [`FlowFabricWorker::claim_from_reclaim_grant`]: https://docs.rs/ff-sdk
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimGrant {
    /// The execution granted for resumption.
    pub execution_id: ExecutionId,
    /// The partition the execution lives on.
    pub partition: crate::partition::Partition,
    /// Valkey key of the grant hash — same key shape as
    /// [`ClaimGrant`].
    pub grant_key: String,
    /// Monotonic ms when the grant expires; unconsumed grants
    /// vanish.
    pub expires_at_ms: u64,
    /// Lane the execution belongs to. Needed by
    /// `ff_claim_resumed_execution` for `KEYS[3]` (eligible_zset)
    /// and `KEYS[9]` (active_index).
    pub lane_id: LaneId,
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
    #[serde(default)]
    pub source: CancelSource,
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
    /// Caller-side timestamp for bookkeeping. NOT passed to the Lua FCALL —
    /// ff_issue_reclaim_grant uses `redis.call("TIME")` for grant_expires_at
    /// (same as ff_issue_claim_grant). Kept for contract symmetry with
    /// IssueClaimGrantArgs and scheduler audit logging.
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueReclaimGrantResult {
    /// Reclaim grant issued.
    Granted { expires_at_ms: TimestampMs },
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
        /// HMAC-SHA1 token bound to (waitpoint_id, waitpoint_key, created_at).
        /// Required by signal-delivery callers to authenticate against this
        /// waitpoint (RFC-004 §Waitpoint Security).
        waitpoint_token: WaitpointToken,
    },
    /// Buffered signals already satisfied the condition — suspension skipped.
    /// Lease is still held. Token comes from the pending waitpoint record.
    AlreadySatisfied {
        suspension_id: SuspensionId,
        waitpoint_id: WaitpointId,
        waitpoint_key: String,
        waitpoint_token: WaitpointToken,
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
        /// HMAC-SHA1 token bound to the pending waitpoint. Required for
        /// `buffer_signal_for_pending_waitpoint` and carried forward when
        /// the waitpoint is activated by `suspend_execution`.
        waitpoint_token: WaitpointToken,
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
    /// HMAC-SHA1 token issued when the waitpoint was created. Required for
    /// signal delivery; missing/tampered/rotated-past-grace tokens are
    /// rejected with `invalid_token` or `token_expired` (RFC-004).
    ///
    /// Defense-in-depth: `WaitpointToken` is a transparent string newtype,
    /// so an empty string deserializes successfully from JSON. The
    /// validation boundary is in Lua (`validate_waitpoint_token` returns
    /// `missing_token` on empty input); this type intentionally does NOT
    /// pre-reject at the Rust layer so callers get a consistent typed
    /// error regardless of how they constructed the args.
    pub waitpoint_token: WaitpointToken,
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
    /// HMAC-SHA1 token issued when `create_pending_waitpoint` ran. Required
    /// to authenticate early signals targeting the pending waitpoint.
    pub waitpoint_token: WaitpointToken,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BufferSignalResult {
    /// Signal buffered for pending waitpoint.
    Buffered { signal_id: SignalId },
    /// Duplicate signal.
    Duplicate { existing_signal_id: SignalId },
}

// ─── list_pending_waitpoints ───

/// One entry in the read-only view of an execution's active waitpoints.
///
/// Returned by `Server::list_pending_waitpoints` (and the
/// `GET /v1/executions/{id}/pending-waitpoints` REST endpoint). The
/// `waitpoint_token` is the same HMAC-SHA1 credential a suspending worker
/// receives in `SuspendOutcome::Suspended` — a reviewer that needs to
/// deliver a signal against this waitpoint must present it in
/// `DeliverSignalArgs::waitpoint_token`.
///
/// Exposing the token here is a deliberate API gap closure: a
/// human-in-the-loop reviewer has no other path to the token, since only
/// the suspending worker sees the `SuspendOutcome`. Access is gated by
/// the same bearer-auth middleware as every other REST endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingWaitpointInfo {
    pub waitpoint_id: WaitpointId,
    pub waitpoint_key: String,
    /// Current waitpoint state: `pending`, `active`, `closed`. Callers
    /// typically filter to `pending` or `active`.
    pub state: String,
    /// HMAC-SHA1 token minted at create time; required by
    /// `ff_deliver_signal` and `ff_buffer_signal_for_pending_waitpoint`.
    pub waitpoint_token: WaitpointToken,
    /// Signal names the resume condition is waiting for. Reviewers that
    /// need to drive a specific waitpoint — particularly when multiple
    /// concurrent waitpoints exist on one execution — filter on this to
    /// pick the right target.
    ///
    /// An EMPTY vec means the condition matches any signal (wildcard, per
    /// `lua/helpers.lua` `initialize_condition`). Callers must not infer
    /// "no waitpoint" from empty; check `state` / length of the outer
    /// list for that.
    #[serde(default)]
    pub required_signal_names: Vec<String>,
    /// Timestamp when the waitpoint record was first written.
    pub created_at: TimestampMs,
    /// Timestamp when the waitpoint was activated (suspension landed).
    /// `None` while the waitpoint is still `pending`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activated_at: Option<TimestampMs>,
    /// Scheduled expiration timestamp. `None` if no timeout configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<TimestampMs>,
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

// ─── read_attempt_stream / tail_attempt_stream ───

/// Hard cap on the number of frames returned by a single read/tail call.
///
/// Single source of truth across the Rust layer (ff-script, ff-server,
/// ff-sdk). The Lua side in `lua/stream.lua` keeps a matching literal with
/// an inline reference back here; bump both together if you ever need to
/// lift the cap.
pub const STREAM_READ_HARD_CAP: u64 = 10_000;

/// A single frame read from an attempt-scoped stream.
///
/// Field set mirrors what `ff_append_frame` writes: `frame_type`, `ts`,
/// `payload`, `encoding`, `source`, and optionally `correlation_id`. Stored
/// as an ordered map so field order is deterministic across read calls.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamFrame {
    /// Valkey Stream entry ID, e.g. "1713100800150-0".
    pub id: String,
    /// Frame fields in sorted order.
    pub fields: std::collections::BTreeMap<String, String>,
}

/// Inputs to `ff_read_attempt_stream` (XRANGE wrapper).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadFramesArgs {
    pub execution_id: ExecutionId,
    pub attempt_index: AttemptIndex,
    /// XRANGE start ID. Use "-" for earliest.
    pub from_id: String,
    /// XRANGE end ID. Use "+" for latest.
    pub to_id: String,
    /// XRANGE COUNT limit. MUST be `>= 1`. The REST and SDK layers reject
    /// `0` at the boundary; the Lua side rejects it too. `STREAM_READ_HARD_CAP`
    /// is the upper bound.
    pub count_limit: u64,
}

/// Result of reading frames from an attempt stream — frames plus terminal
/// signal so consumers can stop polling without a timeout fallback.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamFrames {
    /// Entries in the requested range (possibly empty).
    pub frames: Vec<StreamFrame>,
    /// Timestamp when the upstream writer closed the stream. `None` if the
    /// stream is still open (or has never been written).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<TimestampMs>,
    /// Reason from the closing writer. Current values:
    /// `attempt_success`, `attempt_failure`, `attempt_cancelled`,
    /// `attempt_interrupted`. `None` iff the stream is still open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_reason: Option<String>,
}

impl StreamFrames {
    /// Construct an empty open-stream result (no frames, no terminal
    /// markers). Useful for fast-path peek helpers.
    pub fn empty_open() -> Self {
        Self { frames: Vec::new(), closed_at: None, closed_reason: None }
    }

    /// True iff the producer has closed this stream. Consumers should stop
    /// polling and drain once this returns true.
    pub fn is_closed(&self) -> bool {
        self.closed_at.is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadFramesResult {
    /// Frames returned (possibly empty) plus optional closed markers.
    Frames(StreamFrames),
}

// ═══════════════════════════════════════════════════════════════════════
// Phase 5 contracts: budget, quota, block/unblock
// ═══════════════════════════════════════════════════════════════════════

// ─── create_budget ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateBudgetArgs {
    pub budget_id: crate::types::BudgetId,
    pub scope_type: String,
    pub scope_id: String,
    pub enforcement_mode: String,
    pub on_hard_limit: String,
    pub on_soft_limit: String,
    pub reset_interval_ms: u64,
    /// Dimension names.
    pub dimensions: Vec<String>,
    /// Hard limits per dimension (parallel with dimensions).
    pub hard_limits: Vec<u64>,
    /// Soft limits per dimension (parallel with dimensions).
    pub soft_limits: Vec<u64>,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CreateBudgetResult {
    /// Budget created.
    Created { budget_id: crate::types::BudgetId },
    /// Already exists (idempotent).
    AlreadySatisfied { budget_id: crate::types::BudgetId },
}

// ─── create_quota_policy ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateQuotaPolicyArgs {
    pub quota_policy_id: crate::types::QuotaPolicyId,
    pub window_seconds: u64,
    pub max_requests_per_window: u64,
    pub max_concurrent: u64,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CreateQuotaPolicyResult {
    /// Quota policy created.
    Created { quota_policy_id: crate::types::QuotaPolicyId },
    /// Already exists (idempotent).
    AlreadySatisfied { quota_policy_id: crate::types::QuotaPolicyId },
}

// ─── budget_status (read-only) ───

/// Operator-facing budget status snapshot (not an FCALL — direct HGETALL reads).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetStatus {
    pub budget_id: String,
    pub scope_type: String,
    pub scope_id: String,
    pub enforcement_mode: String,
    /// Current usage per dimension: {dimension_name: current_value}.
    pub usage: HashMap<String, u64>,
    /// Hard limits per dimension: {dimension_name: limit}.
    pub hard_limits: HashMap<String, u64>,
    /// Soft limits per dimension: {dimension_name: limit}.
    pub soft_limits: HashMap<String, u64>,
    pub breach_count: u64,
    pub soft_breach_count: u64,
    pub last_breach_at: Option<String>,
    pub last_breach_dim: Option<String>,
    pub next_reset_at: Option<String>,
    pub created_at: Option<String>,
}

// ─── report_usage_and_check ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReportUsageArgs {
    /// Dimension names to increment.
    pub dimensions: Vec<String>,
    /// Increment values (parallel with dimensions).
    pub deltas: Vec<u64>,
    pub now: TimestampMs,
    /// Optional idempotency key to prevent double-counting on retries.
    /// Must share the budget's `{b:M}` hash tag for cluster safety.
    #[serde(default)]
    pub dedup_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportUsageResult {
    /// All increments applied, no breach.
    Ok,
    /// Soft limit breached on a dimension (advisory, increments applied).
    SoftBreach {
        dimension: String,
        current_usage: u64,
        soft_limit: u64,
    },
    /// Hard limit breached (increments NOT applied).
    HardBreach {
        dimension: String,
        current_usage: u64,
        hard_limit: u64,
    },
    /// Dedup key matched — usage already applied in a prior call.
    AlreadyApplied,
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

// ─── release_admission ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleaseAdmissionArgs {
    pub execution_id: ExecutionId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReleaseAdmissionResult {
    Released,
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

// ─── create_flow ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateFlowArgs {
    pub flow_id: crate::types::FlowId,
    pub flow_kind: String,
    pub namespace: Namespace,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CreateFlowResult {
    /// Flow created successfully.
    Created { flow_id: crate::types::FlowId },
    /// Flow already exists (idempotent).
    AlreadySatisfied { flow_id: crate::types::FlowId },
}

// ─── add_execution_to_flow ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AddExecutionToFlowArgs {
    pub flow_id: crate::types::FlowId,
    pub execution_id: ExecutionId,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AddExecutionToFlowResult {
    /// Execution added to flow.
    Added {
        execution_id: ExecutionId,
        new_node_count: u32,
    },
    /// Already a member (idempotent).
    AlreadyMember {
        execution_id: ExecutionId,
        node_count: u32,
    },
}

// ─── cancel_flow ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CancelFlowArgs {
    pub flow_id: crate::types::FlowId,
    pub reason: String,
    pub cancellation_policy: String,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CancelFlowResult {
    /// Flow cancelled and all member cancellations (if any) have completed
    /// synchronously. Used when `cancellation_policy != "cancel_all"`, when
    /// the flow has no members, when the caller opted into synchronous
    /// dispatch (e.g. `?wait=true`), or when the flow was already in a
    /// terminal state (idempotent retry).
    ///
    /// On the idempotent-retry path `member_execution_ids` may be *capped*
    /// at the server (default 1000 entries) to bound response bandwidth on
    /// flows with very large membership. The first (non-idempotent) call
    /// always returns the full list, so clients that need every member id
    /// should persist the initial response.
    Cancelled {
        cancellation_policy: String,
        member_execution_ids: Vec<String>,
    },
    /// Flow state was flipped to cancelled atomically, but member
    /// cancellations are dispatched asynchronously in the background.
    /// Clients may poll `GET /v1/executions/{id}/state` for each member
    /// execution id to track terminal state.
    CancellationScheduled {
        cancellation_policy: String,
        member_count: u32,
        member_execution_ids: Vec<String>,
    },
}

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
    /// The child execution that receives the dependency.
    pub downstream_execution_id: ExecutionId,
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

// ─── get_execution (full read) ───

/// Full execution info returned by `Server::get_execution`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionInfo {
    pub execution_id: ExecutionId,
    pub namespace: String,
    pub lane_id: String,
    pub priority: i32,
    pub execution_kind: String,
    pub state_vector: StateVector,
    pub public_state: PublicState,
    pub created_at: String,
    /// TimestampMs (ms since epoch) when the execution's first attempt
    /// was started by a worker claim. Empty string until the first
    /// claim lands. Serialised as `Option<String>` so pre-claim reads
    /// deserialise cleanly even if the field is absent from the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// TimestampMs when the execution reached a terminal
    /// `completed`/`failed`/`cancelled`/`expired` state. Empty /
    /// absent while still in flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    pub current_attempt_index: u32,
    pub flow_id: Option<String>,
    pub blocking_detail: String,
}

// ─── set_execution_tags / set_flow_tags (issue #58.4) ───

/// Args for `ff_set_execution_tags`. Tag keys MUST match
/// `^[a-z][a-z0-9_]*\.` — the caller-namespace rule — or the FCALL
/// returns `invalid_tag_key`. Values are arbitrary strings. The map is
/// ordered (`BTreeMap`) so two callers submitting the same logical set
/// of tags produce identical ARGV.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SetExecutionTagsArgs {
    pub execution_id: ExecutionId,
    pub tags: BTreeMap<String, String>,
}

/// Result of `ff_set_execution_tags`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SetExecutionTagsResult {
    /// Tags written. `count` is the number of key-value pairs applied.
    Ok { count: u32 },
}

/// Args for `ff_set_flow_tags`. Same namespace rule as
/// [`SetExecutionTagsArgs`]. The Lua function also lazy-migrates any
/// pre-58.4 reserved-namespace fields stashed inline on `flow_core` into
/// the new tags key.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SetFlowTagsArgs {
    pub flow_id: FlowId,
    pub tags: BTreeMap<String, String>,
}

/// Result of `ff_set_flow_tags`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SetFlowTagsResult {
    /// Tags written. `count` is the number of key-value pairs applied.
    Ok { count: u32 },
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
    use crate::types::FlowId;

    #[test]
    fn create_execution_args_serde() {
        let config = crate::partition::PartitionConfig::default();
        let args = CreateExecutionArgs {
            execution_id: ExecutionId::for_flow(&FlowId::new(), &config),
            namespace: Namespace::new("test"),
            lane_id: LaneId::new("default"),
            execution_kind: "llm_call".to_owned(),
            input_payload: b"hello".to_vec(),
            payload_encoding: Some("json".to_owned()),
            priority: 0,
            creator_identity: "test-user".to_owned(),
            idempotency_key: None,
            tags: HashMap::new(),
            policy: None,
            delay_until: None,
            execution_deadline_at: None,
            partition_id: 42,
            now: TimestampMs::now(),
        };
        let json = serde_json::to_string(&args).unwrap();
        let parsed: CreateExecutionArgs = serde_json::from_str(&json).unwrap();
        assert_eq!(args.execution_id, parsed.execution_id);
    }

    #[test]
    fn claim_result_serde() {
        let config = crate::partition::PartitionConfig::default();
        let result = ClaimExecutionResult::Claimed(ClaimedExecution {
            execution_id: ExecutionId::for_flow(&FlowId::new(), &config),
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

// ─── list_executions ───

/// Summary of an execution for list views.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionSummary {
    pub execution_id: ExecutionId,
    pub namespace: String,
    pub lane_id: String,
    pub execution_kind: String,
    pub public_state: String,
    pub priority: i32,
    pub created_at: String,
}

/// Result of a list_executions query.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ListExecutionsResult {
    pub executions: Vec<ExecutionSummary>,
    pub total_returned: usize,
}

// ─── rotate_waitpoint_hmac_secret ───

/// Args for `ff_rotate_waitpoint_hmac_secret`. Rotates the HMAC signing
/// kid on ONE partition. Callers fan out across every partition themselves
/// (ff-server does the parallel fan-out in `rotate_waitpoint_secret`;
/// direct-Valkey consumers mirror the pattern).
///
/// "now" is derived server-side from `redis.call("TIME")` inside the FCALL
/// (consistency with `validate_waitpoint_token` and flow scanners).
/// `grace_ms` is a duration, not a clock value, so carrying it from the
/// caller is safe.
#[derive(Clone, Debug)]
pub struct RotateWaitpointHmacSecretArgs {
    pub new_kid: String,
    pub new_secret_hex: String,
    /// Grace window in ms. Must be non-negative. Tokens signed by the
    /// outgoing kid remain valid for `grace_ms` after this rotation.
    pub grace_ms: u64,
}

/// Outcome of a single-partition rotation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RotateWaitpointHmacSecretOutcome {
    /// Installed the new kid. `previous_kid` is `None` on bootstrap
    /// (no prior `current_kid`). `gc_count` counts expired kids reaped
    /// during this rotation.
    Rotated {
        previous_kid: Option<String>,
        new_kid: String,
        gc_count: u32,
    },
    /// Exact replay — same kid + same secret already installed. Safe
    /// operator retry; no state change.
    Noop { kid: String },
}

// ─── list_waitpoint_hmac_kids ───

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListWaitpointHmacKidsArgs {}

/// Snapshot of the waitpoint HMAC keystore on ONE partition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitpointHmacKids {
    /// The currently-signing kid. `None` if uninitialized.
    pub current_kid: Option<String>,
    /// Kids that still validate existing tokens but no longer sign
    /// new ones. Order is Lua HGETALL traversal order — callers that
    /// need a stable sort should sort by `expires_at_ms`.
    pub verifying: Vec<VerifyingKid>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyingKid {
    pub kid: String,
    pub expires_at_ms: i64,
}
