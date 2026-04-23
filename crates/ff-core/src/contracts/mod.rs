//! Phase 1 function contracts — Args + Result types for each FCALL.
//!
//! Each Args struct defines the typed inputs to a Valkey Function.
//! Each Result enum defines the possible outcomes (success variants + error codes).

pub mod decode;

use crate::policy::ExecutionPolicy;
use crate::state::{AttemptType, PublicState, StateVector};
use crate::types::{
    AttemptId, AttemptIndex, CancelSource, EdgeId, ExecutionId, FlowId, LaneId, LeaseEpoch,
    LeaseFence, LeaseId, Namespace, SignalId, SuspensionId, TimestampMs, WaitpointId,
    WaitpointToken, WorkerId, WorkerInstanceId,
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
    /// Opaque partition handle for this execution's hash-tag slot.
    ///
    /// Public wire type: consumers pass it back to FlowFabric but
    /// must not parse the interior hash tag for routing decisions.
    /// Internal consumers that need the typed
    /// [`crate::partition::Partition`] call [`Self::partition`].
    pub partition_key: crate::partition::PartitionKey,
    /// The Valkey key holding the grant hash (for the worker to
    /// reference).
    pub grant_key: String,
    /// When the grant expires if not consumed.
    pub expires_at_ms: u64,
}

impl ClaimGrant {
    /// Parse `partition_key` into a typed
    /// [`crate::partition::Partition`]. Intended for internal
    /// consumers (scheduler emitter, SDK worker claim path) that
    /// need the family/index pair. Fails only on malformed keys
    /// (which indicates a producer bug).
    ///
    /// Alias collapse applies: a grant issued against `Execution`
    /// family round-trips to `Flow` (see [`crate::partition::PartitionKey`]
    /// for the rationale — routing is preserved, only the metadata
    /// family label normalises).
    pub fn partition(
        &self,
    ) -> Result<crate::partition::Partition, crate::partition::PartitionKeyParseError> {
        self.partition_key.parse()
    }
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
    /// Opaque partition handle for this execution's hash-tag slot.
    ///
    /// Same wire-opacity contract as [`ClaimGrant::partition_key`].
    /// Internal consumers call [`Self::partition`] for the parsed
    /// form.
    pub partition_key: crate::partition::PartitionKey,
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

impl ReclaimGrant {
    /// Parse `partition_key` into a typed
    /// [`crate::partition::Partition`]. See [`ClaimGrant::partition`]
    /// for the alias-collapse note.
    pub fn partition(
        &self,
    ) -> Result<crate::partition::Partition, crate::partition::PartitionKeyParseError> {
        self.partition_key.parse()
    }
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
    /// RFC #58.5 — fence triple. `Some` for SDK worker paths (standard
    /// stale-lease fence). `None` for operator overrides, in which case
    /// `source` must be `CancelSource::OperatorOverride` or the Lua
    /// returns `fence_required`.
    #[serde(default)]
    pub fence: Option<LeaseFence>,
    pub attempt_index: AttemptIndex,
    #[serde(default)]
    pub result_payload: Option<Vec<u8>>,
    #[serde(default)]
    pub result_encoding: Option<String>,
    /// RFC #58.5 — unfenced-call gate. Ignored when `fence` is `Some`.
    #[serde(default)]
    pub source: CancelSource,
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
    /// RFC #58.5 — fence triple. Required (no operator override path for
    /// renew). `None` returns `fence_required`.
    pub fence: Option<LeaseFence>,
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
    Revoked {
        lease_id: String,
        lease_epoch: String,
    },
    /// Already revoked or expired — no action needed.
    AlreadySatisfied { reason: String },
}

// ─── delay_execution ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DelayExecutionArgs {
    pub execution_id: ExecutionId,
    /// RFC #58.5 — fence triple. `None` requires `source ==
    /// CancelSource::OperatorOverride`.
    #[serde(default)]
    pub fence: Option<LeaseFence>,
    pub attempt_index: AttemptIndex,
    pub delay_until: TimestampMs,
    #[serde(default)]
    pub source: CancelSource,
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
    /// RFC #58.5 — fence triple. `None` requires `source ==
    /// CancelSource::OperatorOverride`.
    #[serde(default)]
    pub fence: Option<LeaseFence>,
    pub attempt_index: AttemptIndex,
    #[serde(default)]
    pub source: CancelSource,
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
    /// RFC #58.5 — fence triple. `None` requires `source ==
    /// CancelSource::OperatorOverride`.
    #[serde(default)]
    pub fence: Option<LeaseFence>,
    pub attempt_index: AttemptIndex,
    pub failure_reason: String,
    pub failure_category: String,
    /// JSON-encoded retry policy (from execution policy). Empty = no retries.
    #[serde(default)]
    pub retry_policy_json: String,
    /// JSON-encoded attempt policy for the next retry attempt.
    #[serde(default)]
    pub next_attempt_policy_json: String,
    #[serde(default)]
    pub source: CancelSource,
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
    /// RFC #58.5 — fence triple. Required (no operator override path for
    /// suspend). `None` returns `fence_required`.
    pub fence: Option<LeaseFence>,
    pub attempt_index: AttemptIndex,
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
    Accepted { signal_id: SignalId, effect: String },
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

// ─── StreamCursor (issue #92) ───

/// Opaque cursor for attempt-stream reads/tails.
///
/// Replaces the bare `&str` / `String` stream-id parameters previously
/// carried on `read_stream` / `tail_stream` / `ReadStreamParams` /
/// `TailStreamParams`. The wire form is a flat string — serde is
/// transparent via `try_from`/`into` — so `?from=start&to=end` and
/// `?after=123-0` continue to work for REST clients.
///
/// # Public wire grammar
///
/// The ONLY accepted tokens are:
///
/// * `"start"` — first entry in the stream (XRANGE `-` equivalent).
///   Valid in `read_stream` / `ReadStreamParams`.
/// * `"end"` — latest entry in the stream (XRANGE `+` equivalent).
///   Valid in `read_stream` / `ReadStreamParams`.
/// * `"<ms>"` or `"<ms>-<seq>"` — a concrete Valkey Stream entry id.
///   Valid everywhere.
///
/// The bare XRANGE/XREAD markers `"-"` and `"+"` are **NOT** accepted
/// on the wire. The opaque `StreamCursor` grammar is the public
/// contract; the Valkey `-`/`+` markers are an internal implementation
/// detail carried only inside the Lua-adjacent [`ReadFramesArgs`] /
/// `xread_block` path via [`StreamCursor::to_wire`].
///
/// For XREAD (tail), the documented "from the beginning" convention is
/// `StreamCursor::At("0-0".into())` — use the convenience constructor
/// [`StreamCursor::from_beginning`] which returns exactly that value.
/// `Start` / `End` are rejected by the SDK's `tail_stream` boundary
/// because XREAD does not accept `-` / `+` as cursors. The
/// [`StreamCursor::is_concrete`] helper centralises this
/// Start/End-vs-At decision for boundary-validation call sites.
///
/// # Why an enum instead of a string
///
/// A string parameter lets malformed ids escape to the Lua/Valkey
/// layer, surfacing as a script error and HTTP 500. An enum with
/// fallible `FromStr` / `TryFrom<String>` catches every malformed input
/// at the wire boundary with a structured error, and prevents bare `-`
/// / `+` from leaking into consumer code as tacit extensions of the
/// public API.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StreamCursor {
    /// First entry in the stream (XRANGE start marker).
    Start,
    /// Latest entry in the stream (XRANGE end marker).
    End,
    /// A concrete Valkey Stream entry id (`<ms>` or `<ms>-<seq>`).
    ///
    /// For XREAD-style tails, the documented "from the beginning"
    /// convention is `At("0-0".to_owned())` — see
    /// [`StreamCursor::from_beginning`].
    At(String),
}

impl StreamCursor {
    /// Convenience constructor for the XREAD-from-beginning convention
    /// (`"0-0"`). XREAD's `last_id` is exclusive, so passing this as
    /// the `after` cursor returns every entry in the stream.
    pub fn from_beginning() -> Self {
        Self::At("0-0".to_owned())
    }

    /// Serde default helper — emits `StreamCursor::Start`. Used as
    /// `#[serde(default = "StreamCursor::start")]` on REST query
    /// structs.
    pub fn start() -> Self {
        Self::Start
    }

    /// Serde default helper — emits `StreamCursor::End`.
    pub fn end() -> Self {
        Self::End
    }

    /// Serde default helper — emits
    /// `StreamCursor::from_beginning()`. Used as the default for
    /// `TailStreamParams::after`.
    pub fn beginning() -> Self {
        Self::from_beginning()
    }

    /// Internal-only: lower the cursor to the XRANGE/XREAD marker
    /// string Valkey expects. `Start → "-"`, `End → "+"`,
    /// `At(s) → s`.
    ///
    /// Used at the ff-script adapter edge (right before constructing
    /// `ReadFramesArgs` or calling `xread_block`) to translate the
    /// opaque wire grammar into the Lua-ABI form. NOT part of the
    /// public wire — do not emit these raw characters to consumers.
    /// Hidden from the generated docs to discourage external use;
    /// external consumers should never need to see the raw `-` / `+`.
    #[doc(hidden)]
    pub fn to_wire(&self) -> &str {
        match self {
            Self::Start => "-",
            Self::End => "+",
            Self::At(s) => s.as_str(),
        }
    }

    /// Internal-only owned variant of [`Self::to_wire`] — moves the
    /// inner `String` out of `At(s)` without cloning. Use at adapter
    /// edges that construct an owned wire string (e.g.
    /// `ReadFramesArgs.from_id`) from a `StreamCursor` that is about
    /// to be dropped.
    #[doc(hidden)]
    pub fn into_wire_string(self) -> String {
        match self {
            Self::Start => "-".to_owned(),
            Self::End => "+".to_owned(),
            Self::At(s) => s,
        }
    }

    /// True iff this cursor is a concrete entry id
    /// (`"<ms>"` / `"<ms>-<seq>"`). False for the open markers
    /// `Start` / `End`.
    ///
    /// Used by boundaries like XREAD (tailing) that do not accept
    /// open markers — rejecting a cursor is equivalent to
    /// `!cursor.is_concrete()`. Centralised here to keep the SDK and
    /// REST guards in lock-step.
    pub fn is_concrete(&self) -> bool {
        matches!(self, Self::At(_))
    }
}

impl std::fmt::Display for StreamCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start => f.write_str("start"),
            Self::End => f.write_str("end"),
            Self::At(s) => f.write_str(s),
        }
    }
}

/// Error produced when parsing a [`StreamCursor`] from a string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamCursorParseError {
    /// Empty input.
    Empty,
    /// Input matched a rejected bare-marker alias (`"-"`, `"+"`).
    /// The public wire requires `"start"` / `"end"`; the raw Valkey
    /// markers are internal-only.
    BareMarkerRejected(String),
    /// Input was neither a recognized keyword nor a well-formed
    /// Stream entry id. Entry ids must match `^\d+(?:-\d+)?$`.
    Malformed(String),
}

impl std::fmt::Display for StreamCursorParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("stream cursor must not be empty"),
            Self::BareMarkerRejected(s) => write!(
                f,
                "bare marker '{s}' is not a valid stream cursor; use 'start' or 'end'"
            ),
            Self::Malformed(s) => write!(
                f,
                "invalid stream cursor '{s}' (expected 'start', 'end', '<ms>', or '<ms>-<seq>')"
            ),
        }
    }
}

impl std::error::Error for StreamCursorParseError {}

/// Shared grammar check — classifies `s` as `Start` / `End` / a
/// concrete-id shape / malformed / empty, WITHOUT allocating. The
/// owned vs borrowed entry points ([`StreamCursor::from_str`],
/// [`StreamCursor::try_from`]) consume this classification and move
/// the owned `String` into `At` when applicable, avoiding a
/// round-trip `String → &str → String::to_owned` for the common
/// REST-query path.
enum StreamCursorClass {
    Start,
    End,
    Concrete,
    BareMarker,
    Empty,
    Malformed,
}

fn classify_stream_cursor(s: &str) -> StreamCursorClass {
    if s.is_empty() {
        return StreamCursorClass::Empty;
    }
    if s == "-" || s == "+" {
        return StreamCursorClass::BareMarker;
    }
    if s == "start" {
        return StreamCursorClass::Start;
    }
    if s == "end" {
        return StreamCursorClass::End;
    }
    if !s.is_ascii() {
        return StreamCursorClass::Malformed;
    }
    let (ms_part, seq_part) = match s.split_once('-') {
        Some((ms, seq)) => (ms, Some(seq)),
        None => (s, None),
    };
    let ms_ok = !ms_part.is_empty() && ms_part.bytes().all(|b| b.is_ascii_digit());
    let seq_ok = seq_part
        .map(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(true);
    if ms_ok && seq_ok {
        StreamCursorClass::Concrete
    } else {
        StreamCursorClass::Malformed
    }
}

impl std::str::FromStr for StreamCursor {
    type Err = StreamCursorParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match classify_stream_cursor(s) {
            StreamCursorClass::Start => Ok(Self::Start),
            StreamCursorClass::End => Ok(Self::End),
            StreamCursorClass::Concrete => Ok(Self::At(s.to_owned())),
            StreamCursorClass::BareMarker => {
                Err(StreamCursorParseError::BareMarkerRejected(s.to_owned()))
            }
            StreamCursorClass::Empty => Err(StreamCursorParseError::Empty),
            StreamCursorClass::Malformed => Err(StreamCursorParseError::Malformed(s.to_owned())),
        }
    }
}

impl TryFrom<String> for StreamCursor {
    type Error = StreamCursorParseError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        // Owned parsing path — the `At` variant moves `s` in directly,
        // avoiding the `&str → String::to_owned` re-allocation that a
        // blind forward to `FromStr::from_str(&s)` would force. Error
        // paths still pay one allocation to describe the offending
        // input.
        match classify_stream_cursor(&s) {
            StreamCursorClass::Start => Ok(Self::Start),
            StreamCursorClass::End => Ok(Self::End),
            StreamCursorClass::Concrete => Ok(Self::At(s)),
            StreamCursorClass::BareMarker => Err(StreamCursorParseError::BareMarkerRejected(s)),
            StreamCursorClass::Empty => Err(StreamCursorParseError::Empty),
            StreamCursorClass::Malformed => Err(StreamCursorParseError::Malformed(s)),
        }
    }
}

impl From<StreamCursor> for String {
    fn from(c: StreamCursor) -> Self {
        c.to_string()
    }
}

impl Serialize for StreamCursor {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for StreamCursor {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::try_from(s).map_err(serde::de::Error::custom)
    }
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
        Self {
            frames: Vec::new(),
            closed_at: None,
            closed_reason: None,
        }
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
    Created {
        quota_policy_id: crate::types::QuotaPolicyId,
    },
    /// Already exists (idempotent).
    AlreadySatisfied {
        quota_policy_id: crate::types::QuotaPolicyId,
    },
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
    /// Pass the raw dedup id (e.g. `"retry-42"`); the typed FCALL wrapper
    /// wraps it into `ff:usagededup:{b:M}:<id>` using the budget
    /// partition's hash tag so it co-locates with the other budget keys
    /// (#108).
    #[serde(default)]
    pub dedup_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
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
    /// `?wait=true` dispatch completed but one or more member cancellations
    /// failed mid-loop (e.g. ghost member, Lua error, transport fault after
    /// retries exhausted). The flow itself is still flipped to cancelled
    /// (atomic Lua already ran); callers SHOULD inspect
    /// `failed_member_execution_ids` and either retry those ids directly
    /// via `cancel_execution` or wait for the cancel-backlog reconciler
    /// to retry them in the background.
    ///
    /// Only emitted by the synchronous wait path
    /// ([`crate::CancelFlowArgs`] via `?wait=true`). The async path returns
    /// [`CancelFlowResult::CancellationScheduled`] and delegates retries
    /// to the reconciler — there is no visible "partial" state on the
    /// async path because the dispatch result is not observed inline.
    PartiallyCancelled {
        cancellation_policy: String,
        /// All member execution ids that the cancel_flow FCALL returned
        /// (i.e. the full membership at the moment of cancellation).
        member_execution_ids: Vec<String>,
        /// Strict subset of `member_execution_ids` whose per-member cancel
        /// FCALL returned an error. Order is deterministic (matches the
        /// iteration order over `member_execution_ids`).
        failed_member_execution_ids: Vec<String>,
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

// ─── describe_execution (issue #58.1) ───

/// Engine-decoupled read-model for one execution.
///
/// Returned by `ff_sdk::FlowFabricWorker::describe_execution`. Consumers
/// consult this struct instead of reaching into Valkey's exec_core hash
/// directly — the engine is free to rename fields or restructure storage
/// under this surface.
///
/// `#[non_exhaustive]` — FF may add fields in minor releases without a
/// semver break. Match with `..` or use field-by-field construction.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExecutionSnapshot {
    pub execution_id: ExecutionId,
    pub flow_id: Option<FlowId>,
    pub lane_id: LaneId,
    pub namespace: Namespace,
    pub public_state: PublicState,
    /// Blocking reason string (e.g. `"waiting_for_worker"`,
    /// `"waiting_for_delay"`, `"waiting_for_dependencies"`). `None` when
    /// the exec_core field is empty.
    pub blocking_reason: Option<String>,
    /// Free-form operator-readable detail explaining `blocking_reason`.
    /// `None` when the exec_core field is empty.
    pub blocking_detail: Option<String>,
    /// Summary of the execution's currently-active attempt. `None` when
    /// no attempt has been started (pre-claim) or when the exec_core
    /// attempt fields are all empty.
    pub current_attempt: Option<AttemptSummary>,
    /// Summary of the execution's currently-held lease. `None` when the
    /// execution is not held by a worker.
    pub current_lease: Option<LeaseSummary>,
    /// The waitpoint this execution is currently suspended on, if any.
    pub current_waitpoint: Option<WaitpointId>,
    pub created_at: TimestampMs,
    /// Timestamp of the last write that mutated exec_core. Engine-maintained.
    pub last_mutation_at: TimestampMs,
    pub total_attempt_count: u32,
    /// Caller-owned labels. The prefix `^[a-z][a-z0-9_]*\.` is reserved for
    /// consumer metadata (e.g. `cairn.task_id`); FF guarantees it will not
    /// write keys matching that shape. FF's own fields stay in snake_case
    /// without dots. Empty when no tags are set.
    pub tags: BTreeMap<String, String>,
}

impl ExecutionSnapshot {
    /// Construct an [`ExecutionSnapshot`]. Present so downstream crates
    /// (ff-sdk's `describe_execution`) can assemble the struct despite
    /// the `#[non_exhaustive]` marker. Prefer adding builder-style
    /// helpers here over loosening `non_exhaustive`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        execution_id: ExecutionId,
        flow_id: Option<FlowId>,
        lane_id: LaneId,
        namespace: Namespace,
        public_state: PublicState,
        blocking_reason: Option<String>,
        blocking_detail: Option<String>,
        current_attempt: Option<AttemptSummary>,
        current_lease: Option<LeaseSummary>,
        current_waitpoint: Option<WaitpointId>,
        created_at: TimestampMs,
        last_mutation_at: TimestampMs,
        total_attempt_count: u32,
        tags: BTreeMap<String, String>,
    ) -> Self {
        Self {
            execution_id,
            flow_id,
            lane_id,
            namespace,
            public_state,
            blocking_reason,
            blocking_detail,
            current_attempt,
            current_lease,
            current_waitpoint,
            created_at,
            last_mutation_at,
            total_attempt_count,
            tags,
        }
    }
}

/// Currently-active attempt summary inside an [`ExecutionSnapshot`].
///
/// `#[non_exhaustive]`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AttemptSummary {
    pub attempt_id: AttemptId,
    pub attempt_index: AttemptIndex,
}

impl AttemptSummary {
    /// Construct an [`AttemptSummary`]. See [`ExecutionSnapshot::new`]
    /// for the rationale — `#[non_exhaustive]` blocks cross-crate
    /// struct-literal construction.
    pub fn new(attempt_id: AttemptId, attempt_index: AttemptIndex) -> Self {
        Self {
            attempt_id,
            attempt_index,
        }
    }
}

/// Currently-held lease summary inside an [`ExecutionSnapshot`].
///
/// `#[non_exhaustive]`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LeaseSummary {
    pub lease_epoch: LeaseEpoch,
    pub worker_instance_id: WorkerInstanceId,
    pub expires_at: TimestampMs,
}

impl LeaseSummary {
    /// Construct a [`LeaseSummary`]. See [`ExecutionSnapshot::new`]
    /// for the rationale.
    pub fn new(
        lease_epoch: LeaseEpoch,
        worker_instance_id: WorkerInstanceId,
        expires_at: TimestampMs,
    ) -> Self {
        Self {
            lease_epoch,
            worker_instance_id,
            expires_at,
        }
    }
}

// ─── Common sub-types ───

// ─── describe_flow (issue #58.2) ───

/// Engine-decoupled read-model for one flow.
///
/// Returned by `ff_sdk::FlowFabricWorker::describe_flow`. Consumers
/// consult this struct instead of reaching into Valkey's flow_core hash
/// directly — the engine is free to rename fields or restructure storage
/// under this surface.
///
/// `#[non_exhaustive]` — FF may add fields in minor releases without a
/// semver break. Match with `..` or use [`FlowSnapshot::new`].
///
/// # `public_flow_state`
///
/// Stored as an engine-written string literal on `flow_core`. Known
/// values today: `open`, `running`, `blocked`, `cancelled`, `completed`,
/// `failed`. Surfaced as `String` (not a typed enum) because FF does
/// not yet expose a `PublicFlowState` type — callers that need to act
/// on specific values should match on the literal. The flow_projector
/// writes a parallel `public_flow_state` into the flow's summary hash;
/// this field reflects the authoritative value on `flow_core`, which
/// is what mutation guards (cancel/add-member) consult.
///
/// # `tags`
///
/// Unlike [`ExecutionSnapshot::tags`] (which has a dedicated tags
/// hash), flow tags live inline on `flow_core`. FF's own fields are
/// snake_case without a `.`; any field whose name starts with
/// `<lowercase>.` (e.g. `cairn.task_id`) is treated as consumer-owned
/// metadata and routed here. An empty map means no namespaced tags
/// were written. The prefix convention mirrors
/// [`ExecutionSnapshot::tags`] — consumers should keep tag keys
/// namespaced (`cairn.*`, `operator.*`, etc.) so future FF field
/// additions don't collide.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct FlowSnapshot {
    pub flow_id: FlowId,
    /// The `flow_kind` literal passed to `create_flow` (e.g. `dag`,
    /// `pipeline`). Preserved as-is; FF does not interpret it.
    pub flow_kind: String,
    pub namespace: Namespace,
    /// Authoritative flow state on `flow_core`. See the struct-level
    /// docs for the set of known values.
    pub public_flow_state: String,
    /// Monotonically increasing revision bumped on every structural
    /// mutation (add-member, stage-edge). Used by optimistic-concurrency
    /// writers via `expected_graph_revision`.
    pub graph_revision: u64,
    /// Number of member executions added so far. Never decremented.
    pub node_count: u32,
    /// Number of dependency edges staged so far. Never decremented.
    pub edge_count: u32,
    pub created_at: TimestampMs,
    /// Timestamp of the last write that mutated `flow_core`.
    /// Engine-maintained.
    pub last_mutation_at: TimestampMs,
    /// When the flow reached a terminal state via `cancel_flow`. `None`
    /// while the flow is live. Only written by the cancel path today;
    /// `completed`/`failed` terminal states do not populate this field
    /// (the flow_projector derives them from membership).
    pub cancelled_at: Option<TimestampMs>,
    /// Operator-supplied reason from the `cancel_flow` call. `None`
    /// when the flow has not been cancelled.
    pub cancel_reason: Option<String>,
    /// The `cancellation_policy` value persisted by `cancel_flow`
    /// (e.g. `cancel_all`, `cancel_flow_only`). `None` for flows
    /// cancelled before this field was persisted, or not yet cancelled.
    pub cancellation_policy: Option<String>,
    /// Consumer-owned namespaced metadata (e.g. `cairn.task_id`). See
    /// the struct-level docs for the routing rule.
    pub tags: BTreeMap<String, String>,
}

impl FlowSnapshot {
    /// Construct a [`FlowSnapshot`]. Present so downstream crates
    /// (ff-sdk's `describe_flow`) can assemble the struct despite the
    /// `#[non_exhaustive]` marker.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        flow_id: FlowId,
        flow_kind: String,
        namespace: Namespace,
        public_flow_state: String,
        graph_revision: u64,
        node_count: u32,
        edge_count: u32,
        created_at: TimestampMs,
        last_mutation_at: TimestampMs,
        cancelled_at: Option<TimestampMs>,
        cancel_reason: Option<String>,
        cancellation_policy: Option<String>,
        tags: BTreeMap<String, String>,
    ) -> Self {
        Self {
            flow_id,
            flow_kind,
            namespace,
            public_flow_state,
            graph_revision,
            node_count,
            edge_count,
            created_at,
            last_mutation_at,
            cancelled_at,
            cancel_reason,
            cancellation_policy,
            tags,
        }
    }
}

// ─── describe_edge / list_*_edges (issue #58.3) ───

/// Engine-decoupled read-model for one dependency edge.
///
/// Returned by `ff_sdk::FlowFabricWorker::describe_edge`,
/// `list_incoming_edges`, and `list_outgoing_edges`. Consumers consult
/// this struct instead of reaching into Valkey's per-flow `edge:` hash
/// directly — the engine is free to rename hash fields or restructure
/// key layout under this surface.
///
/// `#[non_exhaustive]` — FF may add fields in minor releases without a
/// semver break. Match with `..` or use [`EdgeSnapshot::new`].
///
/// # Fields
///
/// The struct mirrors the immutable edge record written by
/// `ff_stage_dependency_edge` (see `lua/flow.lua`). The flow-scoped
/// edge hash is only ever written once, at staging time; per-execution
/// resolution state lives on a separate `dep:<edge_id>` hash and is not
/// surfaced here. The `edge_state` field therefore reflects the
/// staging-time literal (currently `pending`), not the downstream
/// execution's dep-edge state.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct EdgeSnapshot {
    pub edge_id: EdgeId,
    pub flow_id: FlowId,
    pub upstream_execution_id: ExecutionId,
    pub downstream_execution_id: ExecutionId,
    /// The `dependency_kind` literal (e.g. `success_only`) from
    /// `stage_dependency_edge`. Preserved as-is; FF does not interpret
    /// it on reads.
    pub dependency_kind: String,
    /// The satisfaction-condition literal stamped at staging time
    /// (e.g. `all_required`).
    pub satisfaction_condition: String,
    /// Optional opaque handle to a data-passing artifact. `None` when
    /// the stored field is empty (the most common case).
    pub data_passing_ref: Option<String>,
    /// Edge-state literal on the flow-scoped edge hash. Written once
    /// at staging as `pending`; this hash is immutable on the flow
    /// side. Per-execution resolution state is tracked separately on
    /// the child's `dep:<edge_id>` hash.
    pub edge_state: String,
    pub created_at: TimestampMs,
    /// Origin of the edge (e.g. `engine`). Preserved as-is.
    pub created_by: String,
}

/// Direction marker for [`crate::engine_backend::EngineBackend::list_edges`].
///
/// Carries the subject execution whose adjacency side the caller wants
/// to list — mirrors the internal `AdjacencySide + subject_eid` pair
/// the ff-sdk free-fn `list_edges_from_set` already uses. Keeping
/// direction + subject fused in one enum means the trait method has a
/// single `direction` parameter rather than a `(side, eid)` pair, and
/// the backend impl can't forget one of the two.
///
/// * `Outgoing { from_node }` — the caller wants every edge whose
///   `upstream_execution_id == from_node`. Corresponds to the
///   `out:<execution_id>` adjacency SET under the execution's flow
///   partition.
/// * `Incoming { to_node }` — the caller wants every edge whose
///   `downstream_execution_id == to_node`. Corresponds to the
///   `in:<execution_id>` adjacency SET under the execution's flow
///   partition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgeDirection {
    /// Edges leaving `from_node` — `out:` adjacency SET.
    Outgoing {
        /// The subject execution whose outgoing edges to list.
        from_node: ExecutionId,
    },
    /// Edges landing on `to_node` — `in:` adjacency SET.
    Incoming {
        /// The subject execution whose incoming edges to list.
        to_node: ExecutionId,
    },
}

impl EdgeDirection {
    /// Return the subject execution id regardless of direction. Shared
    /// helper for backend impls that need the execution id for the
    /// initial `HGET exec_core.flow_id` lookup (flow routing) before
    /// they know which adjacency SET to read.
    pub fn subject(&self) -> &ExecutionId {
        match self {
            Self::Outgoing { from_node } => from_node,
            Self::Incoming { to_node } => to_node,
        }
    }
}

impl EdgeSnapshot {
    /// Construct an [`EdgeSnapshot`]. Present so downstream crates
    /// (ff-sdk's `describe_edge` / `list_*_edges`) can assemble the
    /// struct despite the `#[non_exhaustive]` marker.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        edge_id: EdgeId,
        flow_id: FlowId,
        upstream_execution_id: ExecutionId,
        downstream_execution_id: ExecutionId,
        dependency_kind: String,
        satisfaction_condition: String,
        data_passing_ref: Option<String>,
        edge_state: String,
        created_at: TimestampMs,
        created_by: String,
    ) -> Self {
        Self {
            edge_id,
            flow_id,
            upstream_execution_id,
            downstream_execution_id,
            dependency_kind,
            satisfaction_condition,
            data_passing_ref,
            edge_state,
            created_at,
            created_by,
        }
    }
}

// ─── list_flows (issue #185) ───

/// Typed flow-lifecycle status surfaced on [`FlowSummary`].
///
/// Mirrors the free-form `public_flow_state` literal that FF's flow
/// lifecycle writes onto `flow_core` (known values: `open`, `running`,
/// `blocked`, `cancelled`, `completed`, `failed` — see [`FlowSnapshot`]).
/// The three "active" runtime states (`open`, `running`, `blocked`)
/// collapse to [`FlowStatus::Active`] here — callers that need the
/// exact runtime sub-state should use [`FlowSnapshot::public_flow_state`]
/// via [`crate::engine_backend::EngineBackend::describe_flow`]. `failed`
/// maps to [`FlowStatus::Failed`].
///
/// `#[non_exhaustive]` so future lifecycle states (if FF introduces
/// any) can be added without a semver break.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FlowStatus {
    /// `open` / `running` / `blocked` — flow is still live on the engine.
    Active,
    /// Terminal success: all members reached a successful terminal state
    /// and the flow projector flipped `public_flow_state` to `completed`.
    Completed,
    /// Terminal failure: one or more members failed and the flow
    /// projector flipped `public_flow_state` to `failed`.
    Failed,
    /// Cancelled by an operator via `cancel_flow`.
    Cancelled,
    /// The stored `public_flow_state` literal is present but not a
    /// known value. The raw literal is preserved on
    /// [`FlowSnapshot::public_flow_state`] — callers that need to act
    /// on it should fall back to [`crate::engine_backend::EngineBackend::describe_flow`].
    Unknown,
}

impl FlowStatus {
    /// Map the raw `public_flow_state` literal stored on `flow_core`
    /// to a typed [`FlowStatus`]. Unknown literals surface as
    /// [`FlowStatus::Unknown`] so the list surface stays forwards-
    /// compatible with future engine-side state additions.
    pub fn from_public_flow_state(raw: &str) -> Self {
        match raw {
            "open" | "running" | "blocked" => Self::Active,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Unknown,
        }
    }
}

/// Lightweight per-flow projection returned by
/// [`crate::engine_backend::EngineBackend::list_flows`].
///
/// Deliberately narrower than [`FlowSnapshot`] — listing pages serve
/// dashboard-style enumerations where the caller does not want to pay
/// for the full `flow_core` hash on every row. Consumers that need
/// revision / node-count / tags / cancel metadata should fan out to
/// [`crate::engine_backend::EngineBackend::describe_flow`] for the
/// specific ids they care about.
///
/// `#[non_exhaustive]` — FF may add fields in minor releases without
/// a semver break. Match with `..` or use [`FlowSummary::new`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct FlowSummary {
    pub flow_id: FlowId,
    /// Timestamp (ms since unix epoch) `flow_core.created_at` was
    /// stamped. Mirrors [`FlowSnapshot::created_at`]; kept typed so
    /// callers that want raw millis can read `.0`.
    pub created_at: TimestampMs,
    /// Typed projection of `flow_core.public_flow_state`. See
    /// [`FlowStatus`] for the mapping.
    pub status: FlowStatus,
}

impl FlowSummary {
    /// Construct a [`FlowSummary`]. Present so downstream crates can
    /// assemble the struct despite the `#[non_exhaustive]` marker.
    pub fn new(flow_id: FlowId, created_at: TimestampMs, status: FlowStatus) -> Self {
        Self {
            flow_id,
            created_at,
            status,
        }
    }
}

/// One page of [`FlowSummary`] rows returned by
/// [`crate::engine_backend::EngineBackend::list_flows`].
///
/// `next_cursor` is `Some(last_flow_id)` when at least one more row
/// may exist on the partition — callers forward it verbatim as the
/// next call's `cursor` argument to continue iteration. `None` means
/// the listing is exhausted. Cursor semantics match the Postgres
/// `WHERE flow_id > $cursor ORDER BY flow_id LIMIT $limit` pattern
/// (see the trait method's rustdoc).
///
/// `#[non_exhaustive]` — FF may add summary-level fields (total count,
/// partition hint) in future minor releases without a semver break.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ListFlowsPage {
    pub flows: Vec<FlowSummary>,
    pub next_cursor: Option<FlowId>,
}

impl ListFlowsPage {
    /// Construct a [`ListFlowsPage`]. Present so downstream crates can
    /// assemble the struct despite the `#[non_exhaustive]` marker.
    pub fn new(flows: Vec<FlowSummary>, next_cursor: Option<FlowId>) -> Self {
        Self { flows, next_cursor }
    }
}

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

    // ── StreamCursor (issue #92) ──

    #[test]
    fn stream_cursor_display_matches_wire_tokens() {
        assert_eq!(StreamCursor::Start.to_string(), "start");
        assert_eq!(StreamCursor::End.to_string(), "end");
        assert_eq!(StreamCursor::At("123".into()).to_string(), "123");
        assert_eq!(StreamCursor::At("123-4".into()).to_string(), "123-4");
    }

    #[test]
    fn stream_cursor_to_wire_maps_to_valkey_markers() {
        assert_eq!(StreamCursor::Start.to_wire(), "-");
        assert_eq!(StreamCursor::End.to_wire(), "+");
        assert_eq!(StreamCursor::At("0-0".into()).to_wire(), "0-0");
        assert_eq!(StreamCursor::At("17-3".into()).to_wire(), "17-3");
    }

    #[test]
    fn stream_cursor_from_str_accepts_wire_tokens() {
        use std::str::FromStr;
        assert_eq!(
            StreamCursor::from_str("start").unwrap(),
            StreamCursor::Start
        );
        assert_eq!(StreamCursor::from_str("end").unwrap(), StreamCursor::End);
        assert_eq!(
            StreamCursor::from_str("123").unwrap(),
            StreamCursor::At("123".into())
        );
        assert_eq!(
            StreamCursor::from_str("0-0").unwrap(),
            StreamCursor::At("0-0".into())
        );
        assert_eq!(
            StreamCursor::from_str("1713100800150-0").unwrap(),
            StreamCursor::At("1713100800150-0".into())
        );
    }

    #[test]
    fn stream_cursor_from_str_rejects_bare_markers() {
        use std::str::FromStr;
        assert!(matches!(
            StreamCursor::from_str("-"),
            Err(StreamCursorParseError::BareMarkerRejected(s)) if s == "-"
        ));
        assert!(matches!(
            StreamCursor::from_str("+"),
            Err(StreamCursorParseError::BareMarkerRejected(s)) if s == "+"
        ));
    }

    #[test]
    fn stream_cursor_from_str_rejects_empty() {
        use std::str::FromStr;
        assert_eq!(
            StreamCursor::from_str(""),
            Err(StreamCursorParseError::Empty)
        );
    }

    #[test]
    fn stream_cursor_from_str_rejects_malformed() {
        use std::str::FromStr;
        for bad in [
            "abc", "-1", "1-", "-1-2", "1-2-3", "1.2", "1 2", "Start", "END",
        ] {
            assert!(
                matches!(
                    StreamCursor::from_str(bad),
                    Err(StreamCursorParseError::Malformed(_))
                ),
                "must reject {bad:?}",
            );
        }
    }

    #[test]
    fn stream_cursor_from_str_rejects_non_ascii() {
        use std::str::FromStr;
        assert!(matches!(
            StreamCursor::from_str("1\u{2013}2"),
            Err(StreamCursorParseError::Malformed(_))
        ));
    }

    #[test]
    fn stream_cursor_serde_round_trip() {
        for c in [
            StreamCursor::Start,
            StreamCursor::End,
            StreamCursor::At("0-0".into()),
            StreamCursor::At("1713100800150-0".into()),
        ] {
            let json = serde_json::to_string(&c).unwrap();
            let back: StreamCursor = serde_json::from_str(&json).unwrap();
            assert_eq!(back, c);
        }
    }

    #[test]
    fn stream_cursor_serializes_as_bare_string() {
        assert_eq!(
            serde_json::to_string(&StreamCursor::Start).unwrap(),
            r#""start""#
        );
        assert_eq!(
            serde_json::to_string(&StreamCursor::End).unwrap(),
            r#""end""#
        );
        assert_eq!(
            serde_json::to_string(&StreamCursor::At("123-0".into())).unwrap(),
            r#""123-0""#
        );
    }

    #[test]
    fn stream_cursor_deserialize_rejects_bare_markers() {
        assert!(serde_json::from_str::<StreamCursor>(r#""-""#).is_err());
        assert!(serde_json::from_str::<StreamCursor>(r#""+""#).is_err());
    }

    #[test]
    fn stream_cursor_from_beginning_is_zero_zero() {
        assert_eq!(
            StreamCursor::from_beginning(),
            StreamCursor::At("0-0".into())
        );
    }

    #[test]
    fn stream_cursor_is_concrete_classifies_variants() {
        assert!(!StreamCursor::Start.is_concrete());
        assert!(!StreamCursor::End.is_concrete());
        assert!(StreamCursor::At("0-0".into()).is_concrete());
        assert!(StreamCursor::At("123-0".into()).is_concrete());
        assert!(StreamCursor::from_beginning().is_concrete());
    }

    #[test]
    fn stream_cursor_into_wire_string_moves_without_cloning() {
        assert_eq!(StreamCursor::Start.into_wire_string(), "-");
        assert_eq!(StreamCursor::End.into_wire_string(), "+");
        assert_eq!(StreamCursor::At("17-3".into()).into_wire_string(), "17-3");
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

// ─── list_lanes (issue #184) ───

/// One page of lane ids returned by
/// [`crate::engine_backend::EngineBackend::list_lanes`].
///
/// Lanes are global (not partition-scoped) — the backend enumerates
/// every registered lane, sorts by [`LaneId`] name, and returns a
/// `limit`-sized slice starting after `cursor` (exclusive).
///
/// `next_cursor` is `Some(last_lane_in_page)` when more pages remain
/// and `None` when the caller has read the final page. Callers that
/// want the full list loop until `next_cursor` is `None`, threading
/// the previous page's `next_cursor` into the next call's `cursor`
/// argument.
///
/// `#[non_exhaustive]` — FF may add fields (e.g. a `total` hint) in
/// minor releases without a semver break.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ListLanesPage {
    /// The lanes in this page, sorted by [`LaneId`] name.
    pub lanes: Vec<LaneId>,
    /// Cursor for the next page (exclusive). `None` ⇒ final page.
    pub next_cursor: Option<LaneId>,
}

impl ListLanesPage {
    /// Construct a [`ListLanesPage`]. Present so downstream crates
    /// (ff-backend-valkey's `list_lanes` impl) can assemble the
    /// struct despite the `#[non_exhaustive]` marker.
    pub fn new(lanes: Vec<LaneId>, next_cursor: Option<LaneId>) -> Self {
        Self { lanes, next_cursor }
    }
}

// ─── list_suspended ───

/// One entry in a [`ListSuspendedPage`] — a suspended execution and
/// the reason it is blocked, answering an operator's "what's this
/// waiting on?" without a follow-up round-trip.
///
/// `reason` carries the free-form `reason_code` recorded by the
/// suspending worker at `lua/suspension.lua` (HSET `suspension:current
/// reason_code`). It is a `String`, not a closed enum: the suspension
/// pipeline accepts arbitrary caller-supplied codes (typical values
/// are `"signal"`, `"timer"`, `"children"`, `"join"`, but consumers
/// embed bespoke codes). A future enum projection can classify
/// known codes once the set is frozen; until then, callers that want
/// structured routing MUST match on the string explicitly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SuspendedExecutionEntry {
    /// Execution currently in `lifecycle_phase=suspended`.
    pub execution_id: ExecutionId,
    /// Score stored on the per-lane suspended ZSET — the scheduled
    /// `timeout_at` in milliseconds, or the `9999999999999` sentinel
    /// when no timeout was set (see `lua/suspension.lua`).
    pub suspended_at_ms: i64,
    /// Free-form reason code from `suspension:current.reason_code`.
    /// Empty string when the suspension hash is absent or does not
    /// carry a `reason_code` field (older records). See the struct
    /// rustdoc for the deliberate-String rationale.
    pub reason: String,
}

impl SuspendedExecutionEntry {
    /// Construct a new entry. Preferred over direct field init for
    /// `#[non_exhaustive]` forward-compat.
    pub fn new(execution_id: ExecutionId, suspended_at_ms: i64, reason: String) -> Self {
        Self {
            execution_id,
            suspended_at_ms,
            reason,
        }
    }
}

/// One cursor-paginated page of suspended executions.
///
/// Pagination is cursor-based (not offset/limit) so a Valkey backend
/// can resume a partition scan from the last seen execution id and a
/// future Postgres backend can do keyset pagination on
/// `executions WHERE state='suspended'`. The cursor is opaque to
/// callers: pass `next_cursor` back as the `cursor` argument to the
/// next [`EngineBackend::list_suspended`] call to fetch the next
/// page. `None` means the stream is exhausted.
///
/// [`EngineBackend::list_suspended`]: crate::engine_backend::EngineBackend::list_suspended
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ListSuspendedPage {
    /// Entries on this page, ordered by ascending `suspended_at_ms`
    /// (timeout order) with `execution_id` as a lex tiebreak.
    pub entries: Vec<SuspendedExecutionEntry>,
    /// Resume-point for the next page. `None` when no further
    /// entries remain in the partition.
    pub next_cursor: Option<ExecutionId>,
}

impl ListSuspendedPage {
    /// Construct a new page. Preferred over direct field init for
    /// `#[non_exhaustive]` forward-compat.
    pub fn new(entries: Vec<SuspendedExecutionEntry>, next_cursor: Option<ExecutionId>) -> Self {
        Self {
            entries,
            next_cursor,
        }
    }
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
