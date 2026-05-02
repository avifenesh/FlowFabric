//! Phase 1 function contracts — Args + Result types for each FCALL.
//!
//! Each Args struct defines the typed inputs to a Valkey Function.
//! Each Result enum defines the possible outcomes (success variants + error codes).

pub mod decode;

use crate::policy::ExecutionPolicy;
use crate::state::{AttemptType, PublicState, StateVector};
use crate::types::{
    AttemptId, AttemptIndex, BudgetId, CancelSource, EdgeId, ExecutionId, FlowId, LaneId, LeaseEpoch,
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

/// Inputs to [`crate::engine_backend::EngineBackend::issue_claim_grant`]
/// — the trait-level entry point v0.12 PR-5 lifted out of the SDK-side
/// `FlowFabricWorker::claim_next` inline helper.
///
/// `#[non_exhaustive]` + `::new` per
/// `feedback_non_exhaustive_needs_constructor`: future fields may be
/// added in minor releases; consumers MUST construct via
/// [`Self::new`] and populate optional fields (`capability_hash`,
/// `route_snapshot_json`, `admission_summary`) by direct field
/// assignment on the returned value. Struct-literal construction is
/// blocked by `#[non_exhaustive]`; `..Default::default()` is not
/// available for the same reason.
///
/// Carries the execution's [`crate::partition::Partition`] so the
/// Valkey backend can derive `exec_core` / `claim_grant` / the lane's
/// `eligible_zset` KEYS without a second round-trip.
///
/// Does NOT derive `Serialize` / `Deserialize` — this is a
/// trait-boundary args struct, not a wire-format type; the
/// `#[non_exhaustive]` marker already blocks cross-crate struct-
/// literal construction, which matters more than JSON round-trip
/// for a scanner hot-path primitive.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct IssueClaimGrantArgs {
    pub execution_id: ExecutionId,
    pub lane_id: LaneId,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    /// Partition context for KEYS derivation. v0.12 PR-5.
    pub partition: crate::partition::Partition,
    pub capability_hash: Option<String>,
    pub route_snapshot_json: Option<String>,
    pub admission_summary: Option<String>,
    /// Capabilities this worker advertises. Serialized as a sorted,
    /// comma-separated string to the Lua FCALL (see scheduling.lua
    /// ff_issue_claim_grant). An empty set matches only executions whose
    /// `required_capabilities` is also empty.
    pub worker_capabilities: BTreeSet<String>,
    pub grant_ttl_ms: u64,
    /// Caller-side timestamp for bookkeeping. NOT passed to the Lua FCALL —
    /// ff_issue_claim_grant uses `redis.call("TIME")` for grant_expires_at.
    pub now: TimestampMs,
}

impl IssueClaimGrantArgs {
    /// Construct an `IssueClaimGrantArgs`. Added alongside
    /// `#[non_exhaustive]` per `feedback_non_exhaustive_needs_constructor`
    /// so the SDK worker (and any future caller) can build the args
    /// without the struct literal becoming a cross-crate breaking
    /// change on every minor release.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        execution_id: ExecutionId,
        lane_id: LaneId,
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        partition: crate::partition::Partition,
        worker_capabilities: BTreeSet<String>,
        grant_ttl_ms: u64,
        now: TimestampMs,
    ) -> Self {
        Self {
            execution_id,
            lane_id,
            worker_id,
            worker_instance_id,
            partition,
            capability_hash: None,
            route_snapshot_json: None,
            admission_summary: None,
            worker_capabilities,
            grant_ttl_ms,
            now,
        }
    }
}

/// Typed outcome of [`crate::engine_backend::EngineBackend::issue_claim_grant`].
///
/// Single-variant today — the Valkey FCALL either writes the grant
/// and returns `Granted`, or the Lua reject (capability mismatch,
/// already-granted, etc.) surfaces as a typed [`crate::engine_error::EngineError`]
/// on the outer `Result`. `#[non_exhaustive]` reserves room for
/// future additive variants without a breaking match-arm churn on
/// consumers.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum IssueClaimGrantOutcome {
    /// Grant issued.
    Granted { execution_id: ExecutionId },
}

/// Legacy name for `IssueClaimGrantOutcome` — retained for
/// `ff-script`'s `FromFcallResult` plumbing. Prefer
/// [`IssueClaimGrantOutcome`] in trait-level code.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueClaimGrantResult {
    /// Grant issued.
    Granted { execution_id: ExecutionId },
}

// ─── scan_eligible_executions + block_route (v0.12 PR-5) ───

/// Inputs to [`crate::engine_backend::EngineBackend::scan_eligible_executions`].
///
/// Lifted from the SDK-side `ZRANGEBYSCORE` inline on the
/// `claim_next` scanner (v0.12 PR-5). The backend reads the lane's
/// eligible ZSET on the given partition and returns up to `limit`
/// execution ids in priority order (Valkey: score = `-(priority *
/// 1e12) + created_at_ms`, so `+inf`-bounded ZRANGEBYSCORE with
/// `LIMIT 0 <limit>` yields highest-priority-first).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ScanEligibleArgs {
    pub lane_id: LaneId,
    pub partition: crate::partition::Partition,
    /// Maximum number of execution ids to return. Backends MAY
    /// return fewer when the partition has less work.
    pub limit: u32,
}

impl ScanEligibleArgs {
    /// Construct a `ScanEligibleArgs`. Added alongside
    /// `#[non_exhaustive]` per
    /// `feedback_non_exhaustive_needs_constructor`.
    pub fn new(
        lane_id: LaneId,
        partition: crate::partition::Partition,
        limit: u32,
    ) -> Self {
        Self {
            lane_id,
            partition,
            limit,
        }
    }
}

/// Inputs to [`crate::engine_backend::EngineBackend::block_route`].
///
/// Lifted from the SDK-side `ff_block_execution_for_admission`
/// inline helper on the `claim_next` scanner (v0.12 PR-5). Moves an
/// execution from the lane's eligible ZSET into its blocked_route
/// ZSET after a `CapabilityMismatch` reject — the engine's unblock
/// scanner promotes blocked_route back to eligible once a worker
/// with matching caps registers.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BlockRouteArgs {
    pub execution_id: ExecutionId,
    pub lane_id: LaneId,
    pub partition: crate::partition::Partition,
    /// Free-form block reason code (e.g. `"waiting_for_capable_worker"`).
    pub reason_code: String,
    /// Human-readable reason detail for operator logs.
    pub reason_detail: String,
    pub now: TimestampMs,
}

impl BlockRouteArgs {
    /// Construct a `BlockRouteArgs`.
    pub fn new(
        execution_id: ExecutionId,
        lane_id: LaneId,
        partition: crate::partition::Partition,
        reason_code: String,
        reason_detail: String,
        now: TimestampMs,
    ) -> Self {
        Self {
            execution_id,
            lane_id,
            partition,
            reason_code,
            reason_detail,
            now,
        }
    }
}

/// Typed outcome of [`crate::engine_backend::EngineBackend::block_route`].
///
/// `LuaRejected` captures the logical-reject case (e.g. the execution
/// went terminal between pick and block — eligible ZSET is left
/// unchanged and the caller should simply `continue` to the next
/// partition). Transport faults surface on the outer `Result` as
/// [`crate::engine_error::EngineError::Transport`]; callers that
/// want the pre-PR-5 "best-effort, log-and-continue" semantic wrap
/// the call in a `match` and swallow non-success variants.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BlockRouteOutcome {
    /// Execution moved from eligible → blocked_route successfully.
    Blocked { execution_id: ExecutionId },
    /// Lua returned a non-success result (e.g. execution went
    /// terminal between pick and block). `message` carries the Lua
    /// reject code for operator visibility.
    LuaRejected { message: String },
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
/// **Lane asymmetry with [`ResumeGrant`]:** `ClaimGrant` does NOT
/// carry `lane_id`. The issuing scheduler's caller already picked
/// a lane (that's how admission reached this grant) and passes it
/// through to `claim_from_grant` as a separate argument. The grant
/// handle stays narrow to what uniquely identifies the admission
/// decision. The matching field on [`ResumeGrant`] is an
/// intentional divergence — see the note on that type.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
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
    /// Construct a fresh-claim grant. Added alongside `#[non_exhaustive]`
    /// per RFC-024 §3.1 + `feedback_non_exhaustive_needs_constructor`.
    pub fn new(
        execution_id: ExecutionId,
        partition_key: crate::partition::PartitionKey,
        grant_key: String,
        expires_at_ms: u64,
    ) -> Self {
        Self {
            execution_id,
            partition_key,
            grant_key,
            expires_at_ms,
        }
    }

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

/// A resume grant issued for a resumed (attempt_interrupted) execution.
///
/// Issued by a producer (typically `ff-scheduler` once a Batch-C
/// reclaim scanner is in place; test fixtures in the interim — no
/// production Rust caller exists in-tree today). Consumed by
/// [`FlowFabricWorker::claim_from_resume_grant`], which calls
/// `ff_claim_resumed_execution` atomically: that FCALL validates the
/// grant, consumes it, and transitions `attempt_interrupted` →
/// `started` while preserving the existing `attempt_index` +
/// `attempt_id` (a resumed execution re-uses its attempt; it does
/// not start a new one).
///
/// **Naming history (RFC-024).** This type was historically called
/// `ReclaimGrant`, but its semantic has always been resume-after-
/// suspend (the routing FCALL is `ff_claim_resumed_execution`, not
/// `ff_reclaim_execution`). RFC-024 PR-A renamed the type to
/// `ResumeGrant` — the name now matches the semantic. RFC-024 PR-B+C
/// dropped the transitional `ReclaimGrant = ResumeGrant` alias and
/// introduced a distinct new [`ReclaimGrant`] for the lease-reclaim
/// path (`reclaim_execution` / `ff_reclaim_execution`).
///
/// Mirrors [`ClaimGrant`] for the resume path. Differences:
///
///   * [`ClaimGrant`] is issued against a freshly-eligible
///     execution and `ff_claim_execution` creates a new attempt.
///   * `ResumeGrant` is issued against an `attempt_interrupted`
///     execution; `ff_claim_resumed_execution` re-uses the existing
///     attempt and bumps the lease epoch.
///
/// The grant itself is written to the same `claim_grant` Valkey key
/// that [`ClaimGrant`] uses; the distinction is which Lua FCALL
/// consumes it (`ff_claim_execution` for new attempts,
/// `ff_claim_resumed_execution` for resumes).
///
/// **Lane asymmetry with [`ClaimGrant`]:** `ResumeGrant` CARRIES
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
/// `FlowFabricWorker::claim_from_resume_grant`). Lives in
/// `ff-core` so neither crate needs a dep on the other.
///
/// [`FlowFabricWorker::claim_from_resume_grant`]: https://docs.rs/ff-sdk
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResumeGrant {
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

impl ResumeGrant {
    /// Construct a resume grant. Added alongside `#[non_exhaustive]`
    /// per RFC-024 §3.1 + `feedback_non_exhaustive_needs_constructor`.
    pub fn new(
        execution_id: ExecutionId,
        partition_key: crate::partition::PartitionKey,
        grant_key: String,
        expires_at_ms: u64,
        lane_id: LaneId,
    ) -> Self {
        Self {
            execution_id,
            partition_key,
            grant_key,
            expires_at_ms,
            lane_id,
        }
    }

    /// Parse `partition_key` into a typed
    /// [`crate::partition::Partition`]. See [`ClaimGrant::partition`]
    /// for the alias-collapse note.
    pub fn partition(
        &self,
    ) -> Result<crate::partition::Partition, crate::partition::PartitionKeyParseError> {
        self.partition_key.parse()
    }
}

/// A lease-reclaim grant issued for an execution in
/// `lease_expired_reclaimable` or `lease_revoked` state (RFC-024 §3.1).
///
/// Distinct from [`ResumeGrant`]: the reclaim grant routes to
/// `ff_reclaim_execution` (Valkey) / the new-attempt reclaim impl
/// (PG/SQLite), which creates a NEW attempt row and bumps the
/// execution's `lease_reclaim_count`. The resume grant, by contrast,
/// re-uses the existing attempt under `ff_claim_resumed_execution`.
///
/// Carries `lane_id` for symmetry with [`ResumeGrant`] — the Lua
/// `ff_reclaim_execution` needs the lane for key construction, and
/// the consuming worker would otherwise pay a round-trip to recover
/// it from `exec_core`.
///
/// Backend impl bodies ship under PR-D (PG) / PR-E (SQLite) / PR-F
/// (Valkey). This PR lands only the type + trait surface; default
/// [`crate::engine_backend::EngineBackend::issue_reclaim_grant`] and
/// [`crate::engine_backend::EngineBackend::reclaim_execution`] return
/// [`crate::engine_error::EngineError::Unavailable`] until each
/// backend PR wires its real body.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReclaimGrant {
    /// The execution granted for lease-reclaim.
    pub execution_id: ExecutionId,
    /// Opaque partition handle for this execution's hash-tag slot.
    pub partition_key: crate::partition::PartitionKey,
    /// Backend-scoped grant key (Valkey key / PG+SQLite
    /// `ff_claim_grant.grant_id`).
    pub grant_key: String,
    /// Monotonic ms when the grant expires; unconsumed grants vanish.
    pub expires_at_ms: u64,
    /// Lane the execution belongs to — needed by
    /// `ff_reclaim_execution` for `KEYS[*]` construction.
    pub lane_id: LaneId,
}

impl ReclaimGrant {
    /// Construct a reclaim grant. Added alongside `#[non_exhaustive]`
    /// per RFC-024 §3.1 + `feedback_non_exhaustive_needs_constructor`.
    pub fn new(
        execution_id: ExecutionId,
        partition_key: crate::partition::PartitionKey,
        grant_key: String,
        expires_at_ms: u64,
        lane_id: LaneId,
    ) -> Self {
        Self {
            execution_id,
            partition_key,
            grant_key,
            expires_at_ms,
            lane_id,
        }
    }

    /// Parse `partition_key` into a typed
    /// [`crate::partition::Partition`].
    pub fn partition(
        &self,
    ) -> Result<crate::partition::Partition, crate::partition::PartitionKeyParseError> {
        self.partition_key.parse()
    }
}

// ─── claim_execution ───

#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
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

impl ClaimExecutionArgs {
    /// Construct a `ClaimExecutionArgs`. Added alongside
    /// `#[non_exhaustive]` per `feedback_non_exhaustive_needs_constructor`
    /// so consumers (SDK worker, backend impls) can still build the
    /// args after the struct was sealed for forward-compat.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        execution_id: ExecutionId,
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        lane_id: LaneId,
        lease_id: LeaseId,
        lease_ttl_ms: u64,
        attempt_id: AttemptId,
        expected_attempt_index: AttemptIndex,
        attempt_policy_json: String,
        attempt_timeout_ms: Option<u64>,
        execution_deadline_at: Option<i64>,
        now: TimestampMs,
    ) -> Self {
        Self {
            execution_id,
            worker_id,
            worker_instance_id,
            lane_id,
            lease_id,
            lease_ttl_ms,
            attempt_id,
            expected_attempt_index,
            attempt_policy_json,
            attempt_timeout_ms,
            execution_deadline_at,
            now,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ClaimedExecution {
    pub execution_id: ExecutionId,
    pub lease_id: LeaseId,
    pub lease_epoch: LeaseEpoch,
    pub attempt_index: AttemptIndex,
    pub attempt_id: AttemptId,
    pub attempt_type: AttemptType,
    pub lease_expires_at: TimestampMs,
    /// Backend-populated attempt handle for this claim (v0.12 PR-5.5).
    /// Valkey fills in an encoded `HandleKind::Fresh`; PG/SQLite are
    /// `Unavailable` on `claim_execution` at runtime per
    /// `project_claim_from_grant_pg_sqlite_gap.md`, so the field stays
    /// a stub on those paths.
    #[serde(default = "crate::backend::stub_handle_fresh")]
    pub handle: crate::backend::Handle,
}

impl ClaimedExecution {
    /// Construct a `ClaimedExecution`. Added alongside
    /// `#[non_exhaustive]` per `feedback_non_exhaustive_needs_constructor`
    /// so consumers (backend impls building a claim outcome) can still
    /// build the struct after it was sealed for forward-compat.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        execution_id: ExecutionId,
        lease_id: LeaseId,
        lease_epoch: LeaseEpoch,
        attempt_index: AttemptIndex,
        attempt_id: AttemptId,
        attempt_type: AttemptType,
        lease_expires_at: TimestampMs,
        handle: crate::backend::Handle,
    ) -> Self {
        Self {
            execution_id,
            lease_id,
            lease_epoch,
            attempt_index,
            attempt_id,
            attempt_type,
            lease_expires_at,
            handle,
        }
    }
}

/// Typed outcome of [`crate::engine_backend::EngineBackend::claim_execution`].
///
/// Single-variant today; `#[non_exhaustive]` reserves room for future
/// outcomes (e.g. an explicit `NoGrant` variant if RFC-024 splits it
/// out of `InvalidClaimGrant`) without a breaking match-arm churn on
/// consumers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
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
#[non_exhaustive]
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
    /// Worker capabilities (parity with `IssueClaimGrantArgs`). The
    /// Lua primitive `ff_issue_reclaim_grant` reads these at ARGV[9].
    /// Populated by the SDK admin path from the registered worker's
    /// `WorkerRegistration::capabilities` per RFC-024 §3.2 (B-2).
    #[serde(default)]
    pub worker_capabilities: BTreeSet<String>,
    /// Caller-side timestamp for bookkeeping. NOT passed to the Lua FCALL —
    /// ff_issue_reclaim_grant uses `redis.call("TIME")` for grant_expires_at
    /// (same as ff_issue_claim_grant). Kept for contract symmetry with
    /// IssueClaimGrantArgs and scheduler audit logging.
    pub now: TimestampMs,
}

impl IssueReclaimGrantArgs {
    /// Construct an `IssueReclaimGrantArgs`. Added alongside
    /// `#[non_exhaustive]` per RFC-024 §3.2 +
    /// `feedback_non_exhaustive_needs_constructor`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        execution_id: ExecutionId,
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        lane_id: LaneId,
        capability_hash: Option<String>,
        grant_ttl_ms: u64,
        route_snapshot_json: Option<String>,
        admission_summary: Option<String>,
        worker_capabilities: BTreeSet<String>,
        now: TimestampMs,
    ) -> Self {
        Self {
            execution_id,
            worker_id,
            worker_instance_id,
            lane_id,
            capability_hash,
            grant_ttl_ms,
            route_snapshot_json,
            admission_summary,
            worker_capabilities,
            now,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueReclaimGrantResult {
    /// Reclaim grant issued.
    Granted { expires_at_ms: TimestampMs },
}

/// Typed outcome of [`crate::engine_backend::EngineBackend::issue_reclaim_grant`]
/// (RFC-024 §3.2).
///
/// Construction surface: backends produce variants; consumers match
/// on variants. No `::new()` — variants ARE the surface.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum IssueReclaimGrantOutcome {
    /// Grant issued — hand the carried [`ReclaimGrant`] to
    /// [`crate::engine_backend::EngineBackend::reclaim_execution`].
    Granted(ReclaimGrant),
    /// Execution is not in a reclaimable state (not
    /// `lease_expired_reclaimable` / `lease_revoked`).
    NotReclaimable {
        execution_id: ExecutionId,
        detail: String,
    },
    /// `max_reclaim_count` exceeded; execution transitioned to
    /// terminal_failed by the backend primitive.
    ReclaimCapExceeded {
        execution_id: ExecutionId,
        reclaim_count: u32,
    },
}

// ─── reclaim_execution ───

#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
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
    /// Maximum reclaim count before terminal failure. `None` ⇒ backend
    /// applies the Rust-surface default of 1000 per RFC-024 §4.6. The
    /// Lua fallback remains 100 for pre-RFC ARGV-omitted call sites;
    /// the two-default coexistence is explicit by design.
    #[serde(default)]
    pub max_reclaim_count: Option<u32>,
    /// Old worker instance (for old_worker_leases key construction).
    pub old_worker_instance_id: WorkerInstanceId,
    /// Current attempt index (for old_attempt/old_stream_meta key construction).
    pub current_attempt_index: AttemptIndex,
}

impl ReclaimExecutionArgs {
    /// Construct a `ReclaimExecutionArgs`. Added alongside
    /// `#[non_exhaustive]` per RFC-024 §3.2 +
    /// `feedback_non_exhaustive_needs_constructor`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        execution_id: ExecutionId,
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        lane_id: LaneId,
        capability_hash: Option<String>,
        lease_id: LeaseId,
        lease_ttl_ms: u64,
        attempt_id: AttemptId,
        attempt_policy_json: String,
        max_reclaim_count: Option<u32>,
        old_worker_instance_id: WorkerInstanceId,
        current_attempt_index: AttemptIndex,
    ) -> Self {
        Self {
            execution_id,
            worker_id,
            worker_instance_id,
            lane_id,
            capability_hash,
            lease_id,
            lease_ttl_ms,
            attempt_id,
            attempt_policy_json,
            max_reclaim_count,
            old_worker_instance_id,
            current_attempt_index,
        }
    }
}

/// Typed outcome of [`crate::engine_backend::EngineBackend::reclaim_execution`]
/// (RFC-024 §3.2).
///
/// Distinct from the wire-level [`ReclaimExecutionResult`]; this enum
/// is the trait-surface shape consumers match on.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReclaimExecutionOutcome {
    /// Execution reclaimed — carries the new-attempt
    /// [`crate::backend::Handle`] (kind = `Reclaimed`).
    Claimed(crate::backend::Handle),
    /// Execution is not in a reclaimable state.
    NotReclaimable {
        execution_id: ExecutionId,
        detail: String,
    },
    /// `max_reclaim_count` exceeded; execution transitioned to
    /// terminal_failed.
    ReclaimCapExceeded {
        execution_id: ExecutionId,
        reclaim_count: u32,
    },
    /// The supplied grant was not found / already consumed / expired.
    GrantNotFound { execution_id: ExecutionId },
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
/// Returned by `EngineBackend::list_pending_waitpoints` (and the
/// `GET /v1/executions/{id}/pending-waitpoints` REST endpoint).
///
/// **RFC-017 §8 schema rewrite (Stage D1).** This struct no longer
/// carries the raw HMAC `waitpoint_token` at the trait boundary — the
/// backend emits only the sanitised `(token_kid, token_fingerprint)`
/// pair. The HTTP handler (see `ff-server::api::list_pending_waitpoints`)
/// wraps the trait response and re-injects the real token on the
/// v0.7.x wire for one-release deprecation warning; the wire field is
/// removed entirely at v0.8.0.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingWaitpointInfo {
    pub waitpoint_id: WaitpointId,
    pub waitpoint_key: String,
    /// Current waitpoint state: `pending`, `active`, `closed`. Callers
    /// typically filter to `pending` or `active`.
    pub state: String,
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
    /// Owning execution — surfaces without a separate lookup.
    pub execution_id: ExecutionId,
    /// HMAC key identifier (the `<kid>` prefix of the stored
    /// `waitpoint_token`). Safe to expose — identifies which signing
    /// key minted the token without revealing the key material.
    pub token_kid: String,
    /// 16-hex-char (8-byte) fingerprint of the HMAC digest. Audit-friendly
    /// handle that correlates across logs without being replayable.
    pub token_fingerprint: String,
}

impl PendingWaitpointInfo {
    /// Construct a `PendingWaitpointInfo` with the 7 required fields.
    /// Optional fields (`activated_at`, `expires_at`) default to
    /// `None`; use [`Self::with_activated_at`] / [`Self::with_expires_at`]
    /// to populate them. `required_signal_names` defaults to empty
    /// (wildcard condition); use [`Self::with_required_signal_names`]
    /// to set it.
    pub fn new(
        waitpoint_id: WaitpointId,
        waitpoint_key: String,
        state: String,
        created_at: TimestampMs,
        execution_id: ExecutionId,
        token_kid: String,
        token_fingerprint: String,
    ) -> Self {
        Self {
            waitpoint_id,
            waitpoint_key,
            state,
            required_signal_names: Vec::new(),
            created_at,
            activated_at: None,
            expires_at: None,
            execution_id,
            token_kid,
            token_fingerprint,
        }
    }

    pub fn with_activated_at(mut self, activated_at: TimestampMs) -> Self {
        self.activated_at = Some(activated_at);
        self
    }

    pub fn with_expires_at(mut self, expires_at: TimestampMs) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    pub fn with_required_signal_names(mut self, names: Vec<String>) -> Self {
        self.required_signal_names = names;
        self
    }
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
#[non_exhaustive]
pub struct ClaimedResumedExecution {
    pub execution_id: ExecutionId,
    pub lease_id: LeaseId,
    pub lease_epoch: LeaseEpoch,
    pub attempt_index: AttemptIndex,
    pub attempt_id: AttemptId,
    pub lease_expires_at: TimestampMs,
    /// Backend-populated attempt handle for this resumed claim
    /// (v0.12 PR-5.5). Valkey fills in `HandleKind::Resumed`; PG/SQLite
    /// populate a backend-tagged real handle via
    /// `ff_core::handle_codec::encode`.
    #[serde(default = "crate::backend::stub_handle_resumed")]
    pub handle: crate::backend::Handle,
}

impl ClaimedResumedExecution {
    /// Construct a `ClaimedResumedExecution`. Added alongside
    /// `#[non_exhaustive]` per `feedback_non_exhaustive_needs_constructor`
    /// so consumers (backend impls building a resumed-claim outcome)
    /// can still build the struct after it was sealed for forward-compat.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        execution_id: ExecutionId,
        lease_id: LeaseId,
        lease_epoch: LeaseEpoch,
        attempt_index: AttemptIndex,
        attempt_id: AttemptId,
        lease_expires_at: TimestampMs,
        handle: crate::backend::Handle,
    ) -> Self {
        Self {
            execution_id,
            lease_id,
            lease_epoch,
            attempt_index,
            attempt_id,
            lease_expires_at,
            handle,
        }
    }
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

/// RFC-017 Stage E2: result of the "header" portion of a cancel_flow
/// operation — the atomic flow-state flip + member enumeration.
///
/// The Server composes this with its own wait/async member-dispatch
/// machinery to build the wire-level [`CancelFlowResult`]. Backends
/// implement [`crate::engine_backend::EngineBackend::cancel_flow_header`]
/// (default: `Unavailable`) so the Valkey-native `ff_cancel_flow`
/// FCALL (with its `flow_already_terminal` idempotency branch) can be
/// driven through the trait without re-shaping the existing public
/// `cancel_flow(id, policy, wait)` signature.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CancelFlowHeader {
    /// Flow-state flipped this call. `member_execution_ids` is the
    /// full (uncapped) membership at flip time.
    Cancelled {
        cancellation_policy: String,
        member_execution_ids: Vec<String>,
    },
    /// Flow was already in a terminal state on entry. The backend has
    /// surfaced the *stored* `cancellation_policy`, `cancel_reason`,
    /// and full membership so the Server can return an idempotent
    /// [`CancelFlowResult::Cancelled`] without re-doing the flip.
    AlreadyTerminal {
        /// `None` only for flows cancelled by pre-E2 Lua that never
        /// persisted the policy field.
        stored_cancellation_policy: Option<String>,
        /// `None` when the flow was never cancel-reason-stamped.
        stored_cancel_reason: Option<String>,
        /// Full membership. Server caps to
        /// `ALREADY_TERMINAL_MEMBER_CAP` before wiring.
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

/// Inputs to [`crate::engine_backend::EngineBackend::resolve_dependency`]
/// — the trait-level entry point PR-7b Step 0 lifted out of the two
/// inline FCALL call sites (`ff-engine::partition_router::
/// dispatch_dependency_resolution` and
/// `ff-engine::scanner::dependency_reconciler::resolve_eligible_edges`).
///
/// Both Valkey call sites build identical KEYS[14]+ARGV[5] arrays.
/// The fields below are the minimum they need to survive a trait-
/// boundary hand-off: `partition` drives the Valkey key-tagging,
/// `downstream_execution_id` + `lane_id` + `current_attempt_index`
/// pin the child's KEYS, `upstream_execution_id` derives KEYS[11]
/// (`upstream_result` for server-side `data_passing_ref` copy),
/// `flow_id` supplies the RFC-016 Stage C ARGV[4] + edgegroup/incoming
/// KEYS.
///
/// `#[non_exhaustive]` + `::new` per
/// `feedback_non_exhaustive_needs_constructor`. Does NOT derive
/// `Serialize` / `Deserialize` — this is a trait-boundary args
/// struct, not a wire-format type.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ResolveDependencyArgs {
    /// Child (downstream) execution's partition. Flow + exec partitions
    /// co-locate on `{fp:N}` post-RFC-011 so the FCALL stays single-slot.
    pub partition: crate::partition::Partition,
    pub flow_id: crate::types::FlowId,
    pub downstream_execution_id: ExecutionId,
    pub upstream_execution_id: ExecutionId,
    pub edge_id: crate::types::EdgeId,
    pub lane_id: LaneId,
    /// Child's current attempt index — selects `attempt_hash` +
    /// `stream_meta` KEYS so late satisfaction updates the active
    /// attempt (race-safe under renewal).
    pub current_attempt_index: AttemptIndex,
    /// "success", "failed", "cancelled", "expired", "skipped"
    pub upstream_outcome: String,
    pub now: TimestampMs,
}

impl ResolveDependencyArgs {
    /// Construct a `ResolveDependencyArgs`. Added alongside
    /// `#[non_exhaustive]` per
    /// `feedback_non_exhaustive_needs_constructor`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        partition: crate::partition::Partition,
        flow_id: crate::types::FlowId,
        downstream_execution_id: ExecutionId,
        upstream_execution_id: ExecutionId,
        edge_id: crate::types::EdgeId,
        lane_id: LaneId,
        current_attempt_index: AttemptIndex,
        upstream_outcome: String,
        now: TimestampMs,
    ) -> Self {
        Self {
            partition,
            flow_id,
            downstream_execution_id,
            upstream_execution_id,
            edge_id,
            lane_id,
            current_attempt_index,
            upstream_outcome,
            now,
        }
    }
}

/// Typed outcome of
/// [`crate::engine_backend::EngineBackend::resolve_dependency`].
///
/// `#[non_exhaustive]` so additional variants (e.g. `ChildSkipped`
/// cascade hints) can be added in minor releases without forcing
/// match-arm churn on consumers.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResolveDependencyOutcome {
    /// Edge satisfied — downstream may become eligible.
    Satisfied,
    /// Edge made impossible — downstream becomes skipped.
    Impossible,
    /// Already resolved (idempotent).
    AlreadyResolved,
}

/// Legacy name retained for `ff-script`'s `FromFcallResult` plumbing.
/// Prefer [`ResolveDependencyOutcome`] in trait-level code.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolveDependencyResult {
    /// Edge satisfied — downstream may become eligible.
    Satisfied,
    /// Edge made impossible — downstream becomes skipped.
    Impossible,
    /// Already resolved (idempotent).
    AlreadyResolved,
}

impl From<ResolveDependencyResult> for ResolveDependencyOutcome {
    fn from(r: ResolveDependencyResult) -> Self {
        match r {
            ResolveDependencyResult::Satisfied => ResolveDependencyOutcome::Satisfied,
            ResolveDependencyResult::Impossible => ResolveDependencyOutcome::Impossible,
            ResolveDependencyResult::AlreadyResolved => ResolveDependencyOutcome::AlreadyResolved,
        }
    }
}

// ─── cascade_completion (PR-7b Cluster 4) ─────────────────────────────

/// Typed outcome of
/// [`crate::engine_backend::EngineBackend::cascade_completion`].
///
/// Observable result of cascading a terminal-execution completion into
/// its downstream edges. Counters are best-effort — they describe what
/// the backend actually did on this call, not an authoritative graph
/// state (the `dependency_reconciler` remains the correctness safety
/// net for both backends).
///
/// # Timing semantics
///
/// The two in-tree backends differ in *when* cascade work is observable
/// to the caller. This is an accepted architectural divergence; consumer
/// code that needs synchronous cascade either targets Valkey explicitly
/// or inspects Postgres's observability surface (outbox drain) to
/// verify.
///
/// - **Valkey (`synchronous = true`):** the FCALL cascade runs
///   inline — when `cascade_completion` returns, every directly
///   resolvable edge has been advanced and every `child_skipped`
///   transitive descendant has been recursively cascaded up to the
///   `MAX_CASCADE_DEPTH` cap. `resolved` + `cascaded_children`
///   reflect the full subtree walked on this invocation.
/// - **Postgres (`synchronous = false`):** the call enqueues a
///   dispatch against the `ff_completion_event` outbox row; actual
///   downstream `ff_edge_group` advancement is performed by
///   `ff_backend_postgres::dispatch::dispatch_completion` in per-hop
///   transactions. `resolved` is the number of edge groups advanced
///   during this invocation's outbox drain; `cascaded_children` is
///   always `0` (PG does not self-recurse — further hops go through
///   their own outbox events emitted by the completing transaction).
///
/// `#[non_exhaustive]` so additional counters (e.g. an explicit
/// `dispatched_event_id` for PG, or a `depth_reached` for Valkey) can
/// be added without breaking consumers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CascadeOutcome {
    /// Edges whose resolution observably advanced on this call.
    pub resolved: usize,
    /// Transitive descendants cascaded synchronously (Valkey only;
    /// always `0` on Postgres — see Timing semantics).
    pub cascaded_children: usize,
    /// `true` when the caller observed the cascade inline (Valkey);
    /// `false` when the call only enqueued dispatch (Postgres outbox).
    pub synchronous: bool,
}

impl CascadeOutcome {
    /// Construct an outcome describing a synchronous (Valkey) cascade.
    pub fn synchronous(resolved: usize, cascaded_children: usize) -> Self {
        Self { resolved, cascaded_children, synchronous: true }
    }

    /// Construct an outcome describing an async (Postgres outbox)
    /// dispatch. `advanced` is the per-call outbox-drain count.
    pub fn async_dispatched(advanced: usize) -> Self {
        Self { resolved: advanced, cascaded_children: 0, synchronous: false }
    }
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
/// `#[non_exhaustive]`. New fields may be added in minor releases — use
/// [`LeaseSummary::new`] plus the fluent `with_*` setters to construct
/// one, and match with `..` in destructuring.
///
/// # Field provenance (FF#278)
///
/// * `lease_id` — minted at claim time (`ff_claim_execution` /
///   `ff_claim_resumed_execution`), cleared atomically with the other
///   lease fields on revoke/expire/complete. Stable for the lifetime
///   of a single lease; a fresh one is minted per re-claim. Defaults
///   to the nil UUID when a backend does not surface per-lease ids
///   (treat as "field not populated").
/// * `attempt_index` — the 1-based attempt counter (`current_attempt_index`
///   on `exec_core`). Set atomically with `current_attempt_id` at claim
///   time; always populated while a Valkey-backed lease is held.
/// * `last_heartbeat_at` — the most recent `ff_renew_lease` timestamp
///   (`lease_last_renewed_at` on `exec_core`). `None` when the field
///   is empty (e.g. legacy data pre-0.9, or a backend that does not
///   surface per-renewal heartbeats).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LeaseSummary {
    pub lease_epoch: LeaseEpoch,
    pub worker_instance_id: WorkerInstanceId,
    pub expires_at: TimestampMs,
    /// Per-lease unique identity. Correlates audit-log entries,
    /// reclaim events, and recovery traces.
    pub lease_id: LeaseId,
    /// 1-based attempt counter; `.0` mirrors `current_attempt_index`
    /// on `exec_core`.
    pub attempt_index: AttemptIndex,
    /// Most recent heartbeat (lease-renewal) timestamp. `None` when the
    /// backend does not surface per-renewal ticks on this lease.
    pub last_heartbeat_at: Option<TimestampMs>,
}

impl LeaseSummary {
    /// Construct a [`LeaseSummary`] with the three always-present
    /// fields. Use the `with_*` setters to populate the FF#278
    /// additions (`lease_id`, `attempt_index`, `last_heartbeat_at`);
    /// otherwise they default to the nil / zero / empty forms, which
    /// callers should treat as "field not surfaced by this backend".
    ///
    /// See [`ExecutionSnapshot::new`] for the broader `#[non_exhaustive]`
    /// construction rationale.
    pub fn new(
        lease_epoch: LeaseEpoch,
        worker_instance_id: WorkerInstanceId,
        expires_at: TimestampMs,
    ) -> Self {
        Self {
            lease_epoch,
            worker_instance_id,
            expires_at,
            lease_id: LeaseId::from_uuid(uuid::Uuid::nil()),
            attempt_index: AttemptIndex::new(0),
            last_heartbeat_at: None,
        }
    }

    /// Set the lease's unique identity (FF#278).
    #[must_use]
    pub fn with_lease_id(mut self, lease_id: LeaseId) -> Self {
        self.lease_id = lease_id;
        self
    }

    /// Set the 1-based attempt counter (FF#278).
    #[must_use]
    pub fn with_attempt_index(mut self, attempt_index: AttemptIndex) -> Self {
        self.attempt_index = attempt_index;
        self
    }

    /// Set the most recent heartbeat timestamp (FF#278).
    #[must_use]
    pub fn with_last_heartbeat_at(mut self, ts: TimestampMs) -> Self {
        self.last_heartbeat_at = Some(ts);
        self
    }
}

// ─── read_execution_context (v0.12 agnostic-SDK prep, PR-1) ───

/// Point-read bundle of the three execution-scoped fields the SDK
/// worker needs to construct a `ClaimedTask` (see `ff_sdk::ClaimedTask`):
/// `input_payload`, `execution_kind`, and `tags`.
///
/// Returned by
/// [`EngineBackend::read_execution_context`](crate::engine_backend::EngineBackend::read_execution_context).
/// All three fields are execution-scoped (not per-attempt) across
/// Valkey, Postgres, and SQLite — there is no per-attempt variant in
/// the data model.
///
/// `#[non_exhaustive]` — FF may add fields in minor releases without a
/// semver break. Construct via [`ExecutionContext::new`]; match with
/// `..` when destructuring.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExecutionContext {
    /// Opaque payload handed to the execution body. Empty when the
    /// execution was created with no payload.
    pub input_payload: Vec<u8>,
    /// Caller-supplied `execution_kind` label — free-form string
    /// identifying which handler the worker should dispatch to.
    pub execution_kind: String,
    /// Caller-owned tag map. Tag key conventions mirror
    /// [`ExecutionSnapshot::tags`]; empty when no tags are set.
    pub tags: HashMap<String, String>,
}

impl ExecutionContext {
    /// Construct an [`ExecutionContext`]. Present so downstream crates
    /// (concrete `EngineBackend` impls) can assemble the struct despite
    /// the `#[non_exhaustive]` marker. Prefer adding builder-style
    /// helpers here over loosening `non_exhaustive`.
    pub fn new(
        input_payload: Vec<u8>,
        execution_kind: String,
        tags: HashMap<String, String>,
    ) -> Self {
        Self {
            input_payload,
            execution_kind,
            tags,
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
    /// RFC-016 Stage A: inbound edge groups known on this flow.
    ///
    /// One entry per downstream execution that has at least one staged
    /// inbound dependency edge. Populated from the
    /// `ff:flow:{fp:N}:<flow_id>:edgegroup:<downstream_eid>` hash —
    /// when that hash is absent (existing flows created before Stage A),
    /// the backend falls back to reading the legacy
    /// `deps_meta.unsatisfied_required_count` counter on the
    /// downstream's exec partition and reports the group as
    /// [`EdgeDependencyPolicy::AllOf`] with the derived counters
    /// (backward-compat shim — see RFC-016 §11 Stage A).
    ///
    /// Every entry in Stage A reports `policy = AllOf`; Stage B/C/D/E
    /// extend the variants and wire the quorum counters.
    pub edge_groups: Vec<EdgeGroupSnapshot>,
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
        edge_groups: Vec<EdgeGroupSnapshot>,
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
            edge_groups,
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

// ─── RFC-016 edge-group policy (Stage A) ───

/// Policy controlling how an inbound edge group's satisfaction is
/// decided.
///
/// Stage A honours only [`EdgeDependencyPolicy::AllOf`] — the two
/// quorum variants exist so the wire/snapshot surface is stable for
/// Stage B/C/D's resolver extensions, but
/// [`crate::engine_backend::EngineBackend::set_edge_group_policy`]
/// rejects them with [`crate::engine_error::EngineError::Validation`]
/// until Stage B lands.
///
/// `#[non_exhaustive]` — future stages may add variants (e.g.
/// `Threshold` — see RFC-016 §10.3) without a semver break. Construct
/// via the [`EdgeDependencyPolicy::all_of`], [`EdgeDependencyPolicy::any_of`],
/// and [`EdgeDependencyPolicy::quorum`] helpers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EdgeDependencyPolicy {
    /// Today's behavior: every edge in the inbound group must be
    /// satisfied (RFC-007 `all_required` + `success_only`).
    AllOf,
    /// k-of-n where k==1 — satisfied on the first upstream success.
    /// Stage A: rejected on
    /// [`crate::engine_backend::EngineBackend::set_edge_group_policy`];
    /// resolver emits nothing for this variant yet.
    AnyOf {
        #[serde(rename = "on_satisfied")]
        on_satisfied: OnSatisfied,
    },
    /// k-of-n quorum. Stage A: rejected on
    /// [`crate::engine_backend::EngineBackend::set_edge_group_policy`].
    Quorum {
        k: u32,
        #[serde(rename = "on_satisfied")]
        on_satisfied: OnSatisfied,
    },
}

impl EdgeDependencyPolicy {
    /// Construct the default all-of policy (RFC-007 behavior).
    pub fn all_of() -> Self {
        Self::AllOf
    }

    /// Construct an any-of policy — reserved for Stage B.
    pub fn any_of(on_satisfied: OnSatisfied) -> Self {
        Self::AnyOf { on_satisfied }
    }

    /// Construct a quorum policy — reserved for Stage B.
    pub fn quorum(k: u32, on_satisfied: OnSatisfied) -> Self {
        Self::Quorum { k, on_satisfied }
    }

    /// Stable string label used for wire format + metric labels.
    /// `all_of` | `any_of` | `quorum`.
    pub fn variant_str(&self) -> &'static str {
        match self {
            Self::AllOf => "all_of",
            Self::AnyOf { .. } => "any_of",
            Self::Quorum { .. } => "quorum",
        }
    }
}

/// Policy for unfinished sibling upstreams once the quorum is met.
///
/// `#[non_exhaustive]` — RFC-016 §10.5 rejects a third variant today
/// but keeps the door open. Construct via [`OnSatisfied::cancel_remaining`]
/// / [`OnSatisfied::let_run`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum OnSatisfied {
    /// Default. Cancel any still-running siblings once quorum met.
    CancelRemaining,
    /// Let stragglers finish; their terminals update counters for
    /// observability only (one-shot downstream).
    LetRun,
}

impl OnSatisfied {
    /// Construct the default `cancel_remaining` disposition.
    pub fn cancel_remaining() -> Self {
        Self::CancelRemaining
    }

    /// Construct the `let_run` disposition.
    pub fn let_run() -> Self {
        Self::LetRun
    }

    /// Stable string label for wire format.
    pub fn variant_str(&self) -> &'static str {
        match self {
            Self::CancelRemaining => "cancel_remaining",
            Self::LetRun => "let_run",
        }
    }
}

/// Edge-group lifecycle state (Stage A exposes only `pending` +
/// `satisfied` + `impossible`; `cancelled` reserved for Stage C).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum EdgeGroupState {
    Pending,
    Satisfied,
    Impossible,
    Cancelled,
}

impl EdgeGroupState {
    pub fn from_literal(s: &str) -> Self {
        match s {
            "satisfied" => Self::Satisfied,
            "impossible" => Self::Impossible,
            "cancelled" => Self::Cancelled,
            _ => Self::Pending,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Satisfied => "satisfied",
            Self::Impossible => "impossible",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Snapshot of one inbound edge group (per downstream execution).
///
/// Exposed via [`FlowSnapshot::edge_groups`]. Stage A only populates
/// `AllOf` groups and their counters; Stage B/C add `failed` /
/// `skipped` / `satisfied_at` wiring for the quorum variants.
///
/// `#[non_exhaustive]` — future stages will add fields (`satisfied_at`,
/// `failed_count` write-path, `cancel_siblings_pending`). Match with
/// `..` or use [`EdgeGroupSnapshot::new`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct EdgeGroupSnapshot {
    pub downstream_execution_id: ExecutionId,
    pub policy: EdgeDependencyPolicy,
    pub total_deps: u32,
    pub satisfied_count: u32,
    pub failed_count: u32,
    pub skipped_count: u32,
    pub running_count: u32,
    pub group_state: EdgeGroupState,
}

impl EdgeGroupSnapshot {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        downstream_execution_id: ExecutionId,
        policy: EdgeDependencyPolicy,
        total_deps: u32,
        satisfied_count: u32,
        failed_count: u32,
        skipped_count: u32,
        running_count: u32,
        group_state: EdgeGroupState,
    ) -> Self {
        Self {
            downstream_execution_id,
            policy,
            total_deps,
            satisfied_count,
            failed_count,
            skipped_count,
            running_count,
            group_state,
        }
    }
}

// ─── set_edge_group_policy (RFC-016 §6.1) ───

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SetEdgeGroupPolicyArgs {
    pub flow_id: FlowId,
    pub downstream_execution_id: ExecutionId,
    pub policy: EdgeDependencyPolicy,
    pub now: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SetEdgeGroupPolicyResult {
    /// Policy stored (fresh write).
    Set,
    /// Policy already stored with an identical value (idempotent).
    AlreadySet,
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

// ─── RFC-025 worker registry ───

/// Inputs to [`crate::engine_backend::EngineBackend::register_worker`]
/// (RFC-025). Feature gate: `core`.
///
/// `worker_instance_id` is process identity (unique per-boot);
/// `worker_id` is pool / logical identity (stable across restarts).
/// See RFC-025 §7 terminology glossary.
///
/// `liveness_ttl_ms` is stored alongside the registration so
/// `heartbeat_worker` refreshes to the same value without the caller
/// re-supplying it.
///
/// Re-registering with the same `worker_instance_id` is
/// **idempotent** (RFC-025 §9.3): caps/lanes/TTL overwritten,
/// `Refreshed` returned. Re-registering with the same
/// `worker_instance_id` under a DIFFERENT `worker_id` is rejected
/// with `Validation(InvalidInput, "instance_id reassigned")`.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisterWorkerArgs {
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    /// Workers serve one-or-more lanes. `BTreeSet` for stable
    /// iteration + dedup.
    pub lanes: BTreeSet<LaneId>,
    pub capabilities: BTreeSet<String>,
    /// Stored for subsequent `heartbeat_worker` TTL refresh.
    pub liveness_ttl_ms: u64,
    pub namespace: Namespace,
    pub now: TimestampMs,
}

impl RegisterWorkerArgs {
    pub fn new(
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        lanes: BTreeSet<LaneId>,
        capabilities: BTreeSet<String>,
        liveness_ttl_ms: u64,
        namespace: Namespace,
        now: TimestampMs,
    ) -> Self {
        Self {
            worker_id,
            worker_instance_id,
            lanes,
            capabilities,
            liveness_ttl_ms,
            namespace,
            now,
        }
    }
}

#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisterWorkerOutcome {
    /// No prior live key for this `worker_instance_id` (fresh boot
    /// or post-TTL-expiry).
    Registered,
    /// Existing live key was found; TTL reset + caps/lanes
    /// overwritten (in-process hot-restart, RFC-025 §9.3).
    Refreshed,
}

/// Inputs to [`crate::engine_backend::EngineBackend::heartbeat_worker`]
/// (RFC-025). Feature gate: `core`.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeartbeatWorkerArgs {
    pub worker_instance_id: WorkerInstanceId,
    pub namespace: Namespace,
    pub now: TimestampMs,
}

impl HeartbeatWorkerArgs {
    pub fn new(
        worker_instance_id: WorkerInstanceId,
        namespace: Namespace,
        now: TimestampMs,
    ) -> Self {
        Self {
            worker_instance_id,
            namespace,
            now,
        }
    }
}

#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeartbeatWorkerOutcome {
    /// TTL refreshed. `next_expiry_ms = now + stored-ttl`; callers
    /// schedule the next heartbeat from this value.
    Refreshed { next_expiry_ms: TimestampMs },
    /// Liveness key was absent — TTL ran out or `mark_worker_dead`
    /// landed earlier. Caller re-registers, not re-heartbeats.
    NotRegistered,
}

/// Inputs to [`crate::engine_backend::EngineBackend::mark_worker_dead`]
/// (RFC-025). Feature gate: `core`.
///
/// `reason` is capped at 256 bytes and must not contain control
/// characters; oversize / invalid reject with
/// `Validation(InvalidInput, "reason: …")`. Mirrors
/// `fail_execution`'s `failure_reason` discipline.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkWorkerDeadArgs {
    pub worker_instance_id: WorkerInstanceId,
    pub namespace: Namespace,
    pub reason: String,
    pub now: TimestampMs,
}

impl MarkWorkerDeadArgs {
    pub fn new(
        worker_instance_id: WorkerInstanceId,
        namespace: Namespace,
        reason: String,
        now: TimestampMs,
    ) -> Self {
        Self {
            worker_instance_id,
            namespace,
            reason,
            now,
        }
    }
}

/// Max bytes for `MarkWorkerDeadArgs.reason`. Mirrors
/// `FailExecutionArgs::failure_reason` ceiling.
pub const MARK_WORKER_DEAD_REASON_MAX_BYTES: usize = 256;

#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkWorkerDeadOutcome {
    Marked,
    /// Liveness key already absent — no-op, idempotent. Unified
    /// variant name with `HeartbeatWorkerOutcome::NotRegistered`.
    NotRegistered,
}

/// Cursor for [`crate::engine_backend::EngineBackend::list_expired_leases`]
/// (RFC-025 §9-locked). Tuple (not bare `ExecutionId`) so pagination
/// is stable under equal-expiry: `ZRANGEBYSCORE` with `LIMIT` keyed
/// on score alone duplicates or skips when two leases share a
/// millisecond.
///
/// Backends order strictly by
/// `(expires_at_ms ASC, execution_id ASC)`.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpiredLeasesCursor {
    pub expires_at_ms: TimestampMs,
    pub execution_id: ExecutionId,
}

impl ExpiredLeasesCursor {
    pub fn new(expires_at_ms: TimestampMs, execution_id: ExecutionId) -> Self {
        Self {
            expires_at_ms,
            execution_id,
        }
    }
}

/// Default fan-out for `list_expired_leases` when
/// `max_partitions_per_call` is `None` (RFC-025 §9.2).
pub const LIST_EXPIRED_LEASES_DEFAULT_MAX_PARTITIONS: u32 = 32;

/// Default page size when `limit` is `None`.
pub const LIST_EXPIRED_LEASES_DEFAULT_LIMIT: u32 = 1_000;

/// Backend cap for `limit`.
pub const LIST_EXPIRED_LEASES_MAX_LIMIT: u32 = 10_000;

/// Inputs to [`crate::engine_backend::EngineBackend::list_expired_leases`]
/// (RFC-025). Feature gate: `suspension`.
///
/// `namespace = None` = cross-namespace sweep for operator tooling
/// (auth enforced at the ff-server admin route, NOT the trait
/// boundary). `Some(ns)` = per-tenant scope.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListExpiredLeasesArgs {
    /// Every returned lease has `expires_at_ms <= as_of`.
    pub as_of: TimestampMs,
    /// Exclusive pagination cursor. `None` = scan from earliest.
    pub after: Option<ExpiredLeasesCursor>,
    /// Default [`LIST_EXPIRED_LEASES_DEFAULT_LIMIT`] when `None`;
    /// backend cap [`LIST_EXPIRED_LEASES_MAX_LIMIT`].
    pub limit: Option<u32>,
    /// Default [`LIST_EXPIRED_LEASES_DEFAULT_MAX_PARTITIONS`] when
    /// `None`.
    pub max_partitions_per_call: Option<u32>,
    pub namespace: Option<Namespace>,
}

impl ListExpiredLeasesArgs {
    pub fn new(as_of: TimestampMs) -> Self {
        Self {
            as_of,
            after: None,
            limit: None,
            max_partitions_per_call: None,
            namespace: None,
        }
    }
}

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpiredLeaseInfo {
    pub execution_id: ExecutionId,
    pub lease_id: LeaseId,
    pub lease_epoch: LeaseEpoch,
    pub worker_instance_id: WorkerInstanceId,
    pub expires_at_ms: TimestampMs,
    pub attempt_index: AttemptIndex,
}

impl ExpiredLeaseInfo {
    pub fn new(
        execution_id: ExecutionId,
        lease_id: LeaseId,
        lease_epoch: LeaseEpoch,
        worker_instance_id: WorkerInstanceId,
        expires_at_ms: TimestampMs,
        attempt_index: AttemptIndex,
    ) -> Self {
        Self {
            execution_id,
            lease_id,
            lease_epoch,
            worker_instance_id,
            expires_at_ms,
            attempt_index,
        }
    }
}

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListExpiredLeasesResult {
    pub entries: Vec<ExpiredLeaseInfo>,
    pub cursor: Option<ExpiredLeasesCursor>,
}

impl ListExpiredLeasesResult {
    pub fn new(entries: Vec<ExpiredLeaseInfo>, cursor: Option<ExpiredLeasesCursor>) -> Self {
        Self { entries, cursor }
    }
}

/// Inputs to [`crate::engine_backend::EngineBackend::list_workers`]
/// (RFC-025 Phase 6, §9.4).
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListWorkersArgs {
    /// `None` = cross-namespace sweep for operator tooling.
    pub namespace: Option<Namespace>,
    /// Exclusive pagination cursor.
    pub after: Option<WorkerInstanceId>,
    /// Default 1000 when `None`.
    pub limit: Option<u32>,
}

impl ListWorkersArgs {
    pub fn new() -> Self {
        Self {
            namespace: None,
            after: None,
            limit: None,
        }
    }
}

impl Default for ListWorkersArgs {
    fn default() -> Self {
        Self::new()
    }
}

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerInfo {
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub namespace: Namespace,
    pub lanes: BTreeSet<LaneId>,
    pub capabilities: BTreeSet<String>,
    pub last_heartbeat_ms: TimestampMs,
    pub liveness_ttl_ms: u64,
    pub registered_at_ms: TimestampMs,
}

impl WorkerInfo {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        namespace: Namespace,
        lanes: BTreeSet<LaneId>,
        capabilities: BTreeSet<String>,
        last_heartbeat_ms: TimestampMs,
        liveness_ttl_ms: u64,
        registered_at_ms: TimestampMs,
    ) -> Self {
        Self {
            worker_id,
            worker_instance_id,
            namespace,
            lanes,
            capabilities,
            last_heartbeat_ms,
            liveness_ttl_ms,
            registered_at_ms,
        }
    }
}

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListWorkersResult {
    pub entries: Vec<WorkerInfo>,
    pub cursor: Option<WorkerInstanceId>,
}

impl ListWorkersResult {
    pub fn new(entries: Vec<WorkerInfo>, cursor: Option<WorkerInstanceId>) -> Self {
        Self { entries, cursor }
    }
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
            handle: crate::backend::stub_handle_fresh(),
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

// ─── list_executions ───

/// One page of partition-scoped execution ids returned by
/// [`EngineBackend::list_executions`](crate::engine_backend::EngineBackend::list_executions).
///
/// Pagination is forward-only and cursor-based. `next_cursor` carries
/// the last `ExecutionId` emitted in `executions` iff another page is
/// available; callers pass that id back as the next call's `cursor`
/// (exclusive start). `next_cursor = None` signals end-of-stream.
///
/// `#[non_exhaustive]` — FF may add fields (e.g. `approximate_total`)
/// in minor releases without a semver break. Use
/// [`ListExecutionsPage::new`] for cross-crate construction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ListExecutionsPage {
    /// Execution ids on this page, in ascending lexicographic order.
    pub executions: Vec<ExecutionId>,
    /// Exclusive cursor to request the next page. `None` ⇒ no more
    /// results.
    pub next_cursor: Option<ExecutionId>,
}

impl ListExecutionsPage {
    /// Construct a [`ListExecutionsPage`]. Present so downstream
    /// crates can assemble the struct despite the `#[non_exhaustive]`
    /// marker.
    pub fn new(executions: Vec<ExecutionId>, next_cursor: Option<ExecutionId>) -> Self {
        Self { executions, next_cursor }
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

// ─── rotate_waitpoint_hmac_secret_all ───

/// Args for [`EngineBackend::rotate_waitpoint_hmac_secret_all`] — the
/// cluster-wide / backend-native rotation of the waitpoint HMAC
/// signing kid.
///
/// **v0.7 migration-master Q4:** a single additive trait method
/// replaces the per-partition fan-out that direct-Valkey consumers
/// hand-rolled via
/// [`ff_sdk::admin::rotate_waitpoint_hmac_secret_all_partitions`].
/// On Valkey it fans out N FCALLs (one per execution partition);
/// on Postgres (Wave 4) it resolves to a single INSERT against the
/// global `ff_waitpoint_hmac(kid, secret, rotated_at)` table (no
/// partition_id column). Consumers prefer this method for clarity —
/// the pre-existing free-fn + per-partition surface stays available
/// for backwards compat.
///
/// `#[non_exhaustive]` with a [`Self::new`] constructor per the
/// project memory-rule (unbuildable non_exhaustive types are a dead
/// API).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RotateWaitpointHmacSecretAllArgs {
    pub new_kid: String,
    pub new_secret_hex: String,
    /// Grace window in ms for tokens signed by the outgoing kid.
    /// Duration (not a clock value), identical to
    /// [`RotateWaitpointHmacSecretArgs::grace_ms`].
    pub grace_ms: u64,
}

impl RotateWaitpointHmacSecretAllArgs {
    /// Build the args. Keeping the constructor so consumers don't
    /// struct-literal past the `#[non_exhaustive]` marker.
    pub fn new(
        new_kid: impl Into<String>,
        new_secret_hex: impl Into<String>,
        grace_ms: u64,
    ) -> Self {
        Self {
            new_kid: new_kid.into(),
            new_secret_hex: new_secret_hex.into(),
            grace_ms,
        }
    }
}

/// Per-partition entry of [`RotateWaitpointHmacSecretAllResult`].
/// Mirrors [`ff_sdk::admin::PartitionRotationOutcome`] but typed at
/// the `ff-core` layer so both Valkey and Postgres backends return
/// the same shape without a Postgres→ferriskey dep.
///
/// On backends with no partition concept (Postgres) the entry list
/// has length 1 with `partition = 0` and the outcome of the global
/// row write.
#[derive(Debug)]
#[non_exhaustive]
pub struct RotateWaitpointHmacSecretAllEntry {
    pub partition: u16,
    /// The per-partition (or global) rotation outcome. Per-partition
    /// failures are surfaced as inner `Err` so the fan-out can report
    /// partial success — matching the existing SDK free-fn contract.
    pub result: Result<RotateWaitpointHmacSecretOutcome, crate::engine_error::EngineError>,
}

impl RotateWaitpointHmacSecretAllEntry {
    pub fn new(
        partition: u16,
        result: Result<RotateWaitpointHmacSecretOutcome, crate::engine_error::EngineError>,
    ) -> Self {
        Self { partition, result }
    }
}

/// Result of [`EngineBackend::rotate_waitpoint_hmac_secret_all`].
///
/// The Valkey backend returns one entry per execution partition. The
/// Postgres backend (Wave 4) will return a single-entry vec with
/// `partition = 0` since the Postgres schema stores one global row
/// per kid (Q4 §adjudication). Consumers that want a uniform "did
/// ALL rotations succeed?" view inspect each entry's `.result`.
#[derive(Debug)]
#[non_exhaustive]
pub struct RotateWaitpointHmacSecretAllResult {
    pub entries: Vec<RotateWaitpointHmacSecretAllEntry>,
}

impl RotateWaitpointHmacSecretAllResult {
    pub fn new(entries: Vec<RotateWaitpointHmacSecretAllEntry>) -> Self {
        Self { entries }
    }
}

// ─── seed_waitpoint_hmac_secret (issue #280) ───

/// Args for [`EngineBackend::seed_waitpoint_hmac_secret`].
///
/// Two required fields and no optional knobs, so there is no fluent
/// builder — just `new(kid, secret_hex)`. `#[non_exhaustive]` is kept
/// (with the paired constructor, per the project memory rule) so
/// future additive knobs don't break callers.
///
/// Boot-time provisioning entry point for fresh deployments — see
/// issue #280 for why cairn needed this in addition to
/// [`RotateWaitpointHmacSecretAllArgs`]. Unlike rotate, seed is
/// idempotent: callers invoke it on every boot and the backend
/// decides whether to install.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SeedWaitpointHmacSecretArgs {
    pub kid: String,
    pub secret_hex: String,
}

impl SeedWaitpointHmacSecretArgs {
    pub fn new(kid: impl Into<String>, secret_hex: impl Into<String>) -> Self {
        Self {
            kid: kid.into(),
            secret_hex: secret_hex.into(),
        }
    }
}

/// Result of [`EngineBackend::seed_waitpoint_hmac_secret`].
///
/// * `Seeded` — the backend had no `current_kid` (or no row in the
///   global keystore) and installed `kid` as the active signing kid.
/// * `AlreadySeeded` — a row for `kid` is already installed.
///   `same_secret` reports whether the stored secret bytes match the
///   caller-supplied hex; `false` means the caller should pick a fresh
///   kid for rotation rather than silently re-installing under the
///   existing kid.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SeedOutcome {
    Seeded { kid: String },
    AlreadySeeded { kid: String, same_secret: bool },
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

// ═══════════════════════════════════════════════════════════════════════
// RFC-013 Stage 1d: EngineBackend::suspend typed args + outcome
// ═══════════════════════════════════════════════════════════════════════
//
// `SuspendExecutionArgs` / `SuspendExecutionResult` above remain the
// wire-level Lua-ARGV mirror used by the backend serializer. The types
// below are the public trait-surface shapes RFC-013 §2.2–§2.6 specifies.
//
// Every type in this block is `#[non_exhaustive]` per the RFC §2.2.1
// memory-rule compliance note; each gets a constructor so external-crate
// consumers can build them without struct-literal access.

use crate::backend::WaitpointHmac;

/// Partition-scoped idempotency key for retry-safe `EngineBackend::suspend`.
///
/// See RFC-013 §2.2 — when set on [`SuspendArgs::idempotency_key`], the
/// backend dedups the call on `(partition, execution_id, idempotency_key)`
/// and a second `suspend` with the same triple returns the first call's
/// [`SuspendOutcome`] verbatim. Absent a key, `suspend` is NOT retry-
/// idempotent; callers must describe-and-reconcile per §3.1.
///
/// Follows the `UsageDimensions::dedup_key` pattern — opaque to the
/// engine, byte-compared at the partition scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    /// Construct from any stringy input. Empty strings are accepted;
    /// the backend treats an empty key as "no dedup" at the serialize
    /// step so `Some(IdempotencyKey::new(""))` is functionally the same
    /// as `None`.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// v1 signal-match predicate inside [`ResumeCondition::Single`].
///
/// RFC-013 §2.4 — `ByName(String)` matches a single concrete signal
/// name; `Wildcard` matches any delivered signal. RFC-014 may extend
/// (payload predicates, pattern matching) — `#[non_exhaustive]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SignalMatcher {
    /// Match by exact signal name.
    ByName(String),
    /// Match any signal delivered to the waitpoint.
    Wildcard,
}

/// Hard cap on composite-condition nesting depth (RFC-014 §5.4
/// invariant 4; §5.5 cap rationale). Soft-cap: bumping requires only
/// this constant + the cap-rationale paragraph in RFC-014 §5.5 — no
/// wire-format change. Keep in sync.
pub const MAX_COMPOSITE_DEPTH: usize = 4;

/// RFC-013 reserves this enum slot; RFC-014 populates it with the
/// concrete composition vocabulary (`AllOf` + `Count`). The enum is
/// `#[non_exhaustive]` so RFC-016 or later RFCs may add variants
/// (`AnyOf` has been explicitly rejected per RFC-014 §2.3 in favour of
/// `Count { n: 1, .. }`; the guard exists for orthogonal future work).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
#[non_exhaustive]
pub enum CompositeBody {
    /// All listed sub-conditions must be satisfied. Order-independent.
    /// Once satisfied, further signals to member waitpoints are observed
    /// but do not re-open satisfaction. RFC-014 §2.1.
    AllOf {
        members: Vec<ResumeCondition>,
    },
    /// At least `n` distinct satisfiers (by [`CountKind`]) must match.
    /// `matcher` optionally constrains participating signals; `None`
    /// lets any signal on any of `waitpoints` count. RFC-014 §2.1.
    Count {
        n: u32,
        count_kind: CountKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        matcher: Option<SignalMatcher>,
        waitpoints: Vec<String>,
    },
}

/// How `Count` nodes distinguish satisfiers. RFC-014 §2.1 + §3.2.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum CountKind {
    /// n distinct `waitpoint_id`s in `waitpoints` must fire.
    DistinctWaitpoints,
    /// n distinct `signal_id`s across the waitpoint set.
    DistinctSignals,
    /// n distinct `source_type:source_identity` tuples.
    DistinctSources,
}

impl CompositeBody {
    /// `AllOf { members }` constructor (RFC-014 §10.3 SDK surface).
    pub fn all_of(members: impl IntoIterator<Item = ResumeCondition>) -> Self {
        Self::AllOf {
            members: members.into_iter().collect(),
        }
    }

    /// `Count` constructor with explicit kind + waitpoint set.
    pub fn count(
        n: u32,
        count_kind: CountKind,
        matcher: Option<SignalMatcher>,
        waitpoints: impl IntoIterator<Item = String>,
    ) -> Self {
        Self::Count {
            n,
            count_kind,
            matcher,
            waitpoints: waitpoints.into_iter().collect(),
        }
    }
}

/// Declarative resume condition for [`SuspendArgs::resume_condition`].
///
/// RFC-013 §2.4 — typed replacement for the SDK's former
/// `ConditionMatcher` / `resume_condition_json` pair.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ResumeCondition {
    /// Single waitpoint-key match with a predicate. `matcher` is
    /// evaluated against every signal delivered to `waitpoint_key`.
    Single {
        waitpoint_key: String,
        matcher: SignalMatcher,
    },
    /// Operator-only resume — no signal satisfies; only an explicit
    /// operator resume closes the waitpoint.
    OperatorOnly,
    /// Pure-timeout suspension. No signal satisfier; the waitpoint
    /// resolves only via `timeout_behavior` at `timeout_at`. Requires
    /// `SuspendArgs::timeout_at` to be `Some(_)` — otherwise the
    /// Rust-side validator rejects as `timeout_only_without_deadline`.
    TimeoutOnly,
    /// Multi-condition composition; RFC-014 defines the body.
    Composite(CompositeBody),
}

/// RFC-014 §5.1 validation error shape. Emitted by
/// [`ResumeCondition::validate_composite`] when a composite fails a
/// structural / cardinality invariant at suspend-time, before any Valkey
/// call. Carries a human-readable `detail` per §5.1.1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompositeValidationError {
    pub detail: String,
}

impl CompositeValidationError {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

impl ResumeCondition {
    /// RFC-014 §10.3 builder — `AllOf` across N distinct waitpoints,
    /// each member a `Single { matcher: Wildcard }` leaf. Canonical
    /// Pattern 3 shape for heterogeneous-subsystem "all fired"
    /// semantics (e.g. `db-migration-complete` + `cache-warmed` +
    /// `feature-flag-set`).
    ///
    /// Callers that need per-waitpoint matchers should construct the
    /// tree directly via
    /// [`ResumeCondition::Composite(CompositeBody::all_of(..))`].
    pub fn all_of_waitpoints<I, S>(waitpoint_keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let members: Vec<ResumeCondition> = waitpoint_keys
            .into_iter()
            .map(|k| ResumeCondition::Single {
                waitpoint_key: k.into(),
                matcher: SignalMatcher::Wildcard,
            })
            .collect();
        ResumeCondition::Composite(CompositeBody::AllOf { members })
    }

    /// Collect every distinct `waitpoint_key` the condition targets.
    /// Used at suspend-time to validate the condition's wp set against
    /// `SuspendArgs.waitpoints` (RFC-014 §5.1 multi-binding cross-
    /// check). Order follows tree DFS, de-duplicated preserving first
    /// occurrence.
    pub fn referenced_waitpoint_keys(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut push = |k: &str| {
            if !out.iter().any(|e| e == k) {
                out.push(k.to_owned());
            }
        };
        fn walk(cond: &ResumeCondition, push: &mut dyn FnMut(&str)) {
            match cond {
                ResumeCondition::Single { waitpoint_key, .. } => push(waitpoint_key),
                ResumeCondition::Composite(body) => walk_body(body, push),
                _ => {}
            }
        }
        fn walk_body(body: &CompositeBody, push: &mut dyn FnMut(&str)) {
            match body {
                CompositeBody::AllOf { members } => {
                    for m in members {
                        walk(m, push);
                    }
                }
                CompositeBody::Count { waitpoints, .. } => {
                    for w in waitpoints {
                        push(w.as_str());
                    }
                }
            }
        }
        walk(self, &mut push);
        out
    }

    /// Validate RFC-014 structural invariants on a composite condition.
    /// Single / OperatorOnly / TimeoutOnly return Ok — they carry no
    /// composite body. Checks cover:
    /// * `AllOf { members: [] }` — §5.1 `allof_empty_members`
    /// * `Count { n: 0 }` — §5.1 `count_n_zero`
    /// * `Count { waitpoints: [] }` — §5.1 `count_waitpoints_empty`
    /// * `Count { n > waitpoints.len(), DistinctWaitpoints }` — §5.1
    ///   `count_exceeds_waitpoint_set`
    /// * depth > [`MAX_COMPOSITE_DEPTH`] — §5.1 `condition_depth_exceeded`
    pub fn validate_composite(&self) -> Result<(), CompositeValidationError> {
        match self {
            ResumeCondition::Composite(body) => validate_body(body, 1, ""),
            _ => Ok(()),
        }
    }
}

fn validate_body(
    body: &CompositeBody,
    depth: usize,
    path: &str,
) -> Result<(), CompositeValidationError> {
    if depth > MAX_COMPOSITE_DEPTH {
        return Err(CompositeValidationError::new(format!(
            "depth {} exceeds cap {} at path {}",
            depth,
            MAX_COMPOSITE_DEPTH,
            if path.is_empty() { "<root>" } else { path }
        )));
    }
    match body {
        CompositeBody::AllOf { members } => {
            if members.is_empty() {
                return Err(CompositeValidationError::new(format!(
                    "allof_empty_members at path {}",
                    if path.is_empty() { "<root>" } else { path }
                )));
            }
            for (i, m) in members.iter().enumerate() {
                let child_path = if path.is_empty() {
                    format!("members[{i}]")
                } else {
                    format!("{path}.members[{i}]")
                };
                if let ResumeCondition::Composite(inner) = m {
                    validate_body(inner, depth + 1, &child_path)?;
                }
                // Leaf `Single` / operator / timeout needs no further
                // structural checks — RFC-013 already constrains them.
            }
            Ok(())
        }
        CompositeBody::Count {
            n,
            count_kind,
            waitpoints,
            ..
        } => {
            if *n == 0 {
                return Err(CompositeValidationError::new(format!(
                    "count_n_zero at path {}",
                    if path.is_empty() { "<root>" } else { path }
                )));
            }
            if waitpoints.is_empty() {
                return Err(CompositeValidationError::new(format!(
                    "count_waitpoints_empty at path {}",
                    if path.is_empty() { "<root>" } else { path }
                )));
            }
            if matches!(count_kind, CountKind::DistinctWaitpoints)
                && (*n as usize) > waitpoints.len()
            {
                return Err(CompositeValidationError::new(format!(
                    "count_exceeds_waitpoint_set: n={} > waitpoints.len()={} at path {}",
                    n,
                    waitpoints.len(),
                    if path.is_empty() { "<root>" } else { path }
                )));
            }
            Ok(())
        }
    }
}

/// Where a satisfied suspension routes back to.
///
/// v1 ships only [`ResumeTarget::Runnable`] — execution returns to
/// `runnable` and goes through normal scheduling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ResumeTarget {
    Runnable,
}

/// Resume-side policy carried alongside [`ResumeCondition`].
///
/// RFC-013 §2.5 — what happens when the condition is satisfied. Fields
/// mirror the `resume_policy_json` the backend serializer writes to Lua
/// (RFC-004 §Resume policy fields).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ResumePolicy {
    pub resume_target: ResumeTarget,
    pub consume_matched_signals: bool,
    pub retain_signal_buffer_until_closed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_delay_ms: Option<u64>,
    pub close_waitpoint_on_resume: bool,
}

impl Default for ResumePolicy {
    fn default() -> Self {
        Self::normal()
    }
}

impl ResumePolicy {
    /// Construct a [`ResumePolicy`] with the canonical v1 defaults
    /// (see [`Self::normal`]). Alias for [`Self::normal`] — provided
    /// so external consumers have a conventional `new` constructor
    /// against this `#[non_exhaustive]` struct.
    pub fn new() -> Self {
        Self::normal()
    }

    /// Canonical v1 defaults (RFC-013 §2.2.1):
    /// * `resume_target = Runnable`
    /// * `consume_matched_signals = true`
    /// * `retain_signal_buffer_until_closed = false`
    /// * `resume_delay_ms = None`
    /// * `close_waitpoint_on_resume = true`
    pub fn normal() -> Self {
        Self {
            resume_target: ResumeTarget::Runnable,
            consume_matched_signals: true,
            retain_signal_buffer_until_closed: false,
            resume_delay_ms: None,
            close_waitpoint_on_resume: true,
        }
    }
}

/// Timeout behavior at the suspension deadline (RFC-004 §Timeout Behavior).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum TimeoutBehavior {
    Fail,
    Cancel,
    Expire,
    AutoResumeWithTimeoutSignal,
    /// v2 per RFC-004 Implementation Notes; enum slot present for
    /// additive RFC-014/RFC-015 landing.
    Escalate,
}

impl TimeoutBehavior {
    /// Lua-side string encoding. Matches the wire values Lua's
    /// `ff_expire_suspension` matches on.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::Fail => "fail",
            Self::Cancel => "cancel",
            Self::Expire => "expire",
            Self::AutoResumeWithTimeoutSignal => "auto_resume_with_timeout_signal",
            Self::Escalate => "escalate",
        }
    }
}

/// Reason category for a suspension (RFC-004 §Suspension Reason Categories).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

impl SuspensionReasonCode {
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::WaitingForSignal => "waiting_for_signal",
            Self::WaitingForApproval => "waiting_for_approval",
            Self::WaitingForCallback => "waiting_for_callback",
            Self::WaitingForToolResult => "waiting_for_tool_result",
            Self::WaitingForOperatorReview => "waiting_for_operator_review",
            Self::PausedByPolicy => "paused_by_policy",
            Self::PausedByBudget => "paused_by_budget",
            Self::StepBoundary => "step_boundary",
            Self::ManualPause => "manual_pause",
        }
    }
}

/// Who requested the suspension.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SuspensionRequester {
    Worker,
    Operator,
    Policy,
    SystemTimeoutPolicy,
}

impl SuspensionRequester {
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Operator => "operator",
            Self::Policy => "policy",
            Self::SystemTimeoutPolicy => "system_timeout_policy",
        }
    }
}

/// How the waitpoint resource backing a [`SuspendArgs`] is obtained.
///
/// RFC-013 §2.2 — `Fresh` mints a new waitpoint as part of `suspend`;
/// `UsePending` activates a waitpoint previously issued via
/// `EngineBackend::create_waitpoint`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

impl WaitpointBinding {
    /// Mint a fresh binding with a random `waitpoint_id` (UUID v4) and
    /// `waitpoint_key = "wpk:<uuid>"`.
    pub fn fresh() -> Self {
        let wp_id = WaitpointId::new();
        let key = format!("wpk:{wp_id}");
        Self::Fresh {
            waitpoint_id: wp_id,
            waitpoint_key: key,
        }
    }

    /// Construct a `UsePending` binding from a pending waitpoint
    /// previously issued by `create_waitpoint`. The HMAC token is
    /// resolved Lua-side from the partition's waitpoint hash at
    /// `suspend` time (RFC-013 §5.1).
    pub fn use_pending(pending: &crate::backend::PendingWaitpoint) -> Self {
        Self::UsePending {
            waitpoint_id: pending.waitpoint_id.clone(),
        }
    }
}

/// Trait-surface input to [`EngineBackend::suspend`] (RFC-013 §2.2 +
/// RFC-014 Pattern 3 widening).
///
/// Built via [`SuspendArgs::new`] + `with_*` setters; direct struct-
/// literal construction across crate boundaries is not possible
/// (`#[non_exhaustive]`).
///
/// ## Waitpoints
///
/// `waitpoints` is a non-empty `Vec<WaitpointBinding>`. The first entry
/// is the "primary" binding (accessible via [`primary`](Self::primary))
/// and carries the `current_waitpoint_id` written onto `exec_core` for
/// operator visibility. Additional entries land in Valkey as their own
/// waitpoint hashes / signal streams / HMAC tokens, enabling RFC-014
/// Pattern 3 `AllOf { members: [Single{wp1}, Single{wp2}, ...] }` across
/// distinct heterogeneous subsystems.
///
/// [`SuspendArgs::new`] takes exactly the primary binding; call
/// [`with_waitpoint`](Self::with_waitpoint) to append further bindings
/// (the RFC-014 builder API).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SuspendArgs {
    pub suspension_id: SuspensionId,
    /// RFC-014 Pattern 3: all waitpoint bindings for this suspension.
    /// Guaranteed non-empty; `waitpoints[0]` is the primary.
    pub waitpoints: Vec<WaitpointBinding>,
    pub resume_condition: ResumeCondition,
    pub resume_policy: ResumePolicy,
    pub reason_code: SuspensionReasonCode,
    pub requested_by: SuspensionRequester,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_at: Option<TimestampMs>,
    pub timeout_behavior: TimeoutBehavior,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_metadata_pointer: Option<String>,
    pub now: TimestampMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<IdempotencyKey>,
}

impl SuspendArgs {
    /// Build a minimal `SuspendArgs` for a worker-originated suspension.
    ///
    /// `waitpoint` becomes the primary binding. Append additional
    /// bindings with [`with_waitpoint`](Self::with_waitpoint) (RFC-014
    /// Pattern 3) or replace the set with
    /// [`with_waitpoints`](Self::with_waitpoints).
    ///
    /// Defaults: `requested_by = Worker`, `timeout_at = None`,
    /// `timeout_behavior = Fail`, `continuation_metadata_pointer = None`,
    /// `idempotency_key = None`.
    pub fn new(
        suspension_id: SuspensionId,
        waitpoint: WaitpointBinding,
        resume_condition: ResumeCondition,
        resume_policy: ResumePolicy,
        reason_code: SuspensionReasonCode,
        now: TimestampMs,
    ) -> Self {
        Self {
            suspension_id,
            waitpoints: vec![waitpoint],
            resume_condition,
            resume_policy,
            reason_code,
            requested_by: SuspensionRequester::Worker,
            timeout_at: None,
            timeout_behavior: TimeoutBehavior::Fail,
            continuation_metadata_pointer: None,
            now,
            idempotency_key: None,
        }
    }

    /// Primary binding — `waitpoints[0]`. Guaranteed present by
    /// construction.
    pub fn primary(&self) -> &WaitpointBinding {
        &self.waitpoints[0]
    }

    pub fn with_timeout(mut self, at: TimestampMs, behavior: TimeoutBehavior) -> Self {
        self.timeout_at = Some(at);
        self.timeout_behavior = behavior;
        self
    }

    pub fn with_requester(mut self, requester: SuspensionRequester) -> Self {
        self.requested_by = requester;
        self
    }

    pub fn with_continuation_metadata_pointer(mut self, p: String) -> Self {
        self.continuation_metadata_pointer = Some(p);
        self
    }

    pub fn with_idempotency_key(mut self, key: IdempotencyKey) -> Self {
        self.idempotency_key = Some(key);
        self
    }

    /// RFC-014 Pattern 3 — append a further waitpoint binding to this
    /// suspension. Each additional binding yields its own waitpoint
    /// hash, signal stream, condition hash and HMAC token in Valkey,
    /// but all share the suspension record and composite evaluator
    /// under one `suspension:current`.
    ///
    /// Ordering: the primary (from [`SuspendArgs::new`]) stays at
    /// `waitpoints[0]`; subsequent `with_waitpoint` calls append at the
    /// tail.
    pub fn with_waitpoint(mut self, binding: WaitpointBinding) -> Self {
        self.waitpoints.push(binding);
        self
    }

    /// RFC-014 Pattern 3 — replace the full binding vector in one call.
    /// Must be non-empty; an empty Vec is a programmer error and will
    /// be rejected by the backend's `validate_suspend_args` with
    /// `waitpoints_empty`.
    pub fn with_waitpoints(mut self, bindings: Vec<WaitpointBinding>) -> Self {
        self.waitpoints = bindings;
        self
    }
}

/// Shared "what happened on the waitpoint" payload carried in both
/// [`SuspendOutcome`] variants.
///
/// For Pattern 3 (RFC-014) — multi-waitpoint suspensions — the primary
/// binding's identity lives at the top level (`waitpoint_id` /
/// `waitpoint_key` / `waitpoint_token`) and remaining bindings are
/// exposed via `additional_waitpoints`, each carrying its own minted
/// HMAC token so external signallers can deliver to any of the N
/// waitpoints the suspension is listening on.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SuspendOutcomeDetails {
    pub suspension_id: SuspensionId,
    pub waitpoint_id: WaitpointId,
    pub waitpoint_key: String,
    pub waitpoint_token: WaitpointHmac,
    /// RFC-014 Pattern 3 extras (beyond the primary). Empty for
    /// single-waitpoint suspensions (patterns 1 + 2); carries one
    /// entry per additional binding for Pattern 3.
    pub additional_waitpoints: Vec<AdditionalWaitpointBinding>,
}

/// RFC-014 Pattern 3 — per-binding identity + HMAC token for
/// waitpoints beyond the primary. Structure mirrors the top-level
/// fields on [`SuspendOutcomeDetails`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AdditionalWaitpointBinding {
    pub waitpoint_id: WaitpointId,
    pub waitpoint_key: String,
    pub waitpoint_token: WaitpointHmac,
}

impl AdditionalWaitpointBinding {
    pub fn new(
        waitpoint_id: WaitpointId,
        waitpoint_key: String,
        waitpoint_token: WaitpointHmac,
    ) -> Self {
        Self {
            waitpoint_id,
            waitpoint_key,
            waitpoint_token,
        }
    }
}

impl SuspendOutcomeDetails {
    pub fn new(
        suspension_id: SuspensionId,
        waitpoint_id: WaitpointId,
        waitpoint_key: String,
        waitpoint_token: WaitpointHmac,
    ) -> Self {
        Self {
            suspension_id,
            waitpoint_id,
            waitpoint_key,
            waitpoint_token,
            additional_waitpoints: Vec::new(),
        }
    }

    /// Attach RFC-014 Pattern 3 additional-waitpoint bindings. The
    /// primary binding stays at the top-level fields; `extras` lands
    /// in [`additional_waitpoints`](Self::additional_waitpoints).
    pub fn with_additional_waitpoints(
        mut self,
        extras: Vec<AdditionalWaitpointBinding>,
    ) -> Self {
        self.additional_waitpoints = extras;
        self
    }
}

/// Trait-surface output from [`EngineBackend::suspend`] (RFC-013 §2.3).
///
/// Two variants encode the "lease released" vs "lease retained" split.
/// See §2.3 for the runtime-enforcement semantics.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SuspendOutcome {
    /// The worker's pre-suspend handle is no longer lease-bearing; a
    /// fresh `HandleKind::Suspended` handle supersedes it.
    Suspended {
        details: SuspendOutcomeDetails,
        handle: crate::backend::Handle,
    },
    /// Buffered signals on a pending waitpoint already satisfied the
    /// condition at suspension time; the lease is retained and the
    /// caller's pre-suspend handle remains valid.
    AlreadySatisfied { details: SuspendOutcomeDetails },
}

impl SuspendOutcome {
    /// Borrow the shared details regardless of variant.
    pub fn details(&self) -> &SuspendOutcomeDetails {
        match self {
            Self::Suspended { details, .. } => details,
            Self::AlreadySatisfied { details } => details,
        }
    }
}

// `EngineBackend::suspend` type re-exports for `ff_core::backend::*`
// consumers. The `backend` module re-exports these below so external
// crates can reach them via the idiomatic `ff_core::backend` path that
// already sources the other trait-surface types (RFC-013 §9.1).

// ─── RFC-017 Stage A — trait-expansion Args/Result types ─────────────
//
// Per RFC-017 §5.1.1: every struct/enum introduced here is
// `#[non_exhaustive]` and ships with a `pub fn new(...)` constructor so
// additive field growth post-v0.8 does not force cross-crate churn.

// ─── claim_for_worker ───

/// Inputs to `EngineBackend::claim_for_worker` (RFC-017 §5, §7). The
/// Valkey impl forwards to `ff_scheduler::Scheduler::claim_for_worker`;
/// the Postgres impl forwards to its own scheduler module. The trait
/// method hides the backend-specific dispatch behind one shape.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct ClaimForWorkerArgs {
    pub lane_id: LaneId,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub worker_capabilities: std::collections::BTreeSet<String>,
    pub grant_ttl_ms: u64,
}

impl ClaimForWorkerArgs {
    /// Required-field constructor. Optional fields today: none — kept
    /// for forward-compat so a future optional (e.g. `deadline_ms`)
    /// does not break callers using the builder pattern.
    pub fn new(
        lane_id: LaneId,
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        worker_capabilities: std::collections::BTreeSet<String>,
        grant_ttl_ms: u64,
    ) -> Self {
        Self {
            lane_id,
            worker_id,
            worker_instance_id,
            worker_capabilities,
            grant_ttl_ms,
        }
    }
}

/// Outcome of `EngineBackend::claim_for_worker`. `None`-like shape
/// modelled as an enum so additive variants (e.g. `BackPressured {
/// retry_after_ms }`) do not force a wire break.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimForWorkerOutcome {
    /// No eligible execution on this lane at this scan cycle.
    NoWork,
    /// Grant issued — worker proceeds to `claim_from_grant`.
    Granted(ClaimGrant),
}

impl ClaimForWorkerOutcome {
    /// Build the `NoWork` variant.
    pub fn no_work() -> Self {
        Self::NoWork
    }
    /// Build the `Granted` variant.
    pub fn granted(grant: ClaimGrant) -> Self {
        Self::Granted(grant)
    }
}

// ─── list_pending_waitpoints ───

/// Inputs to `EngineBackend::list_pending_waitpoints` (RFC-017 §5, §8).
/// Pagination is part of the signature so a flow with 10k pending
/// waitpoints cannot force a single-round-trip read regardless of
/// backend.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct ListPendingWaitpointsArgs {
    pub execution_id: ExecutionId,
    /// Exclusive cursor — `None` starts from the beginning.
    pub after: Option<WaitpointId>,
    /// Max page size. `None` → backend default (100). Backend-enforced
    /// cap: 1000.
    pub limit: Option<u32>,
}

impl ListPendingWaitpointsArgs {
    pub fn new(execution_id: ExecutionId) -> Self {
        Self {
            execution_id,
            after: None,
            limit: None,
        }
    }
    pub fn with_after(mut self, after: WaitpointId) -> Self {
        self.after = Some(after);
        self
    }
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }
}

/// Page of pending-waitpoint entries. Stage A preserves the existing
/// `PendingWaitpointInfo` shape; the §8 schema rewrite (HMAC
/// sanitisation + `(token_kid, token_fingerprint)` additive fields)
/// ships in Stage D alongside the HTTP wire-format deprecation.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct ListPendingWaitpointsResult {
    pub entries: Vec<PendingWaitpointInfo>,
    /// Forward-only continuation cursor — `None` signals end-of-stream.
    pub next_cursor: Option<WaitpointId>,
}

impl ListPendingWaitpointsResult {
    pub fn new(entries: Vec<PendingWaitpointInfo>) -> Self {
        Self {
            entries,
            next_cursor: None,
        }
    }
    pub fn with_next_cursor(mut self, cursor: WaitpointId) -> Self {
        self.next_cursor = Some(cursor);
        self
    }
}

// ─── report_usage_admin ───

/// Inputs to `EngineBackend::report_usage_admin` (RFC-017 §5 budget+
/// quota admin §5, round-1 F4). Admin-path peer of `report_usage` —
/// both wrap `ff_report_usage_and_check` on the Valkey side but the
/// admin call is worker-less, so it cannot reuse the lease-bound
/// `report_usage(&Handle, ...)` signature. `ReportUsageAdminArgs`
/// carries the same fields as [`ReportUsageArgs`] without a worker
/// handle — kept as a distinct type so future admin-only fields (e.g.
/// `actor_identity`, `audit_reason`) don't pollute the worker path.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct ReportUsageAdminArgs {
    pub dimensions: Vec<String>,
    pub deltas: Vec<u64>,
    pub dedup_key: Option<String>,
    pub now: TimestampMs,
}

impl ReportUsageAdminArgs {
    pub fn new(dimensions: Vec<String>, deltas: Vec<u64>, now: TimestampMs) -> Self {
        Self {
            dimensions,
            deltas,
            dedup_key: None,
            now,
        }
    }
    pub fn with_dedup_key(mut self, key: String) -> Self {
        self.dedup_key = Some(key);
        self
    }
}

// ─── #454 — cairn ControlPlaneBackend peer methods ──────────────────
//
// Four trait methods from cairn-rs #454 that previously routed through
// raw `ferriskey::*` FCALLs outside the `EngineBackend` trait. Cairn's
// ground-truth shapes at `cairn-fabric/src/engine/control_plane_types.rs`
// (commit `a4fdb638`) are mirrored below verbatim so the v0.13 trait
// matches cairn's existing Valkey impls 1:1.
//
// Default bodies on the trait return `EngineError::Unavailable { op }`
// at landing; Valkey bodies ship in Phase 3; PG + SQLite in Phases 4+5.

// ─── record_spend ───

/// Args for [`crate::engine_backend::EngineBackend::record_spend`].
///
/// Carries an **open-set** `BTreeMap<String, u64>` of dimension deltas
/// per cairn's ground-truth shape at
/// `cairn-fabric/src/engine/control_plane_types.rs`. Cairn budgets are
/// per-tenant open-schema (tenant A tracks `"tokens"` + `"cost_cents"`,
/// tenant B tracks `"egress_bytes"`), distinct from FF's fixed-shape
/// [`UsageDimensions`] which encodes the internal usage-report surface.
///
/// `BTreeMap` (not `HashMap`) gives stable iteration order — consistent
/// with `UsageDimensions::custom`, and critical for the PG body which
/// updates multiple dimension rows per call (deterministic ordering
/// prevents deadlocks under concurrent spend).
///
/// Return shape reuses [`ReportUsageResult`] — same four variants
/// (`Ok` / `SoftBreach` / `HardBreach` / `AlreadyApplied`) cairn's UI
/// branches on. Not a new enum.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RecordSpendArgs {
    pub budget_id: BudgetId,
    pub execution_id: ExecutionId,
    /// Per-dimension positive deltas. Tenant-defined keys; stable
    /// iteration order.
    pub deltas: BTreeMap<String, u64>,
    /// Caller-computed idempotency key (cairn uses SHA-256 hex of
    /// `budget_id || execution_id || sorted(deltas)`). FF does not
    /// interpret the bytes — dedup is a simple equality check against
    /// the prior stamped key.
    pub idempotency_key: String,
}

impl RecordSpendArgs {
    pub fn new(
        budget_id: BudgetId,
        execution_id: ExecutionId,
        deltas: BTreeMap<String, u64>,
        idempotency_key: impl Into<String>,
    ) -> Self {
        Self {
            budget_id,
            execution_id,
            deltas,
            idempotency_key: idempotency_key.into(),
        }
    }
}

// ─── release_budget ───

/// Args for [`crate::engine_backend::EngineBackend::release_budget`].
///
/// **Per-execution release-my-attribution**, not whole-budget flush.
/// Called when an execution terminates so the budget persists across
/// executions but this execution's attribution is reversed. Per cairn
/// clarification on #454.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ReleaseBudgetArgs {
    pub budget_id: BudgetId,
    pub execution_id: ExecutionId,
}

impl ReleaseBudgetArgs {
    pub fn new(budget_id: BudgetId, execution_id: ExecutionId) -> Self {
        Self {
            budget_id,
            execution_id,
        }
    }
}

// ─── deliver_approval_signal ───

/// Args for [`crate::engine_backend::EngineBackend::deliver_approval_signal`].
///
/// Pre-shaped variant of [`crate::engine_backend::EngineBackend::deliver_signal`]
/// for the operator-driven approval flow. Distinct from `deliver_signal`
/// because the caller **does not carry the waitpoint token** — the backend
/// reads the token from `ff_waitpoint_pending` (via
/// [`crate::engine_backend::EngineBackend::read_waitpoint_token`],
/// #434-shipped in v0.12), HMAC-verifies server-side, and dispatches. The
/// operator API never handles the token bytes.
///
/// `signal_name` is a flat string (`"approved"` / `"rejected"` by
/// convention; not an enum at the trait level — audit metadata like
/// `decided_by` / `note` / `reason` sits in cairn's audit log, not in
/// the FF signal surface).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DeliverApprovalSignalArgs {
    pub execution_id: ExecutionId,
    pub lane_id: LaneId,
    pub waitpoint_id: WaitpointId,
    /// Conventional values: `"approved"` / `"rejected"`. Stored raw on
    /// the delivered signal; FF does not interpret.
    pub signal_name: String,
    /// Cairn-side per-decision idempotency suffix. Combined with
    /// `execution_id` + `signal_name` to form the dedup key.
    pub idempotency_suffix: String,
    /// Dedup TTL in milliseconds.
    pub signal_dedup_ttl_ms: u64,
    /// Signal stream MAXLEN for the suspension stream.
    /// `None` ⇒ backend default (matches [`DeliverSignalArgs::signal_maxlen`]).
    #[serde(default)]
    pub maxlen: Option<u64>,
    /// Per-execution max signal cap (operator quota).
    /// `None` ⇒ backend default (matches [`DeliverSignalArgs::max_signals_per_execution`]).
    #[serde(default)]
    pub max_signals_per_execution: Option<u64>,
}

impl DeliverApprovalSignalArgs {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        execution_id: ExecutionId,
        lane_id: LaneId,
        waitpoint_id: WaitpointId,
        signal_name: impl Into<String>,
        idempotency_suffix: impl Into<String>,
        signal_dedup_ttl_ms: u64,
        maxlen: Option<u64>,
        max_signals_per_execution: Option<u64>,
    ) -> Self {
        Self {
            execution_id,
            lane_id,
            waitpoint_id,
            signal_name: signal_name.into(),
            idempotency_suffix: idempotency_suffix.into(),
            signal_dedup_ttl_ms,
            maxlen,
            max_signals_per_execution,
        }
    }
}

// ─── issue_grant_and_claim ───

/// Args for [`crate::engine_backend::EngineBackend::issue_grant_and_claim`].
///
/// Composes `issue_claim_grant` + `claim_execution` into a single
/// backend-atomic op per cairn #454 Q4. The composition **must** be
/// backend-atomic (not caller-chained) to prevent leaking grants when
/// `claim_execution` fails after `issue_claim_grant` succeeded.
///
/// Valkey: one `ff_issue_grant_and_claim` FCALL composing the two
/// primitives in Lua.
/// Postgres/SQLite: both primitives inside one tx.
///
/// Flattened shape (not `IssueClaimGrantArgs + ClaimExecutionArgs`
/// composition) — the two arg types overlap on `execution_id` +
/// `lane_id`; flattening drops the dup.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct IssueGrantAndClaimArgs {
    pub execution_id: ExecutionId,
    pub lane_id: LaneId,
    /// Lease TTL in milliseconds. Threaded into both the grant TTL and
    /// the claimed attempt's `lease_expires_at_ms`.
    pub lease_duration_ms: u64,
}

impl IssueGrantAndClaimArgs {
    pub fn new(execution_id: ExecutionId, lane_id: LaneId, lease_duration_ms: u64) -> Self {
        Self {
            execution_id,
            lane_id,
            lease_duration_ms,
        }
    }
}

/// Outcome of [`crate::engine_backend::EngineBackend::issue_grant_and_claim`].
///
/// Distinct from [`ClaimExecutionResult`] because the trait method
/// intentionally hides the grant-issuance step — callers only see the
/// resulting lease identity. If the backend's transparent dispatch
/// routes through `ff_claim_resumed_execution` (when the execution was
/// suspended), the return is identical.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ClaimGrantOutcome {
    pub lease_id: LeaseId,
    pub lease_epoch: LeaseEpoch,
    pub attempt_index: AttemptIndex,
}

impl ClaimGrantOutcome {
    pub fn new(
        lease_id: LeaseId,
        lease_epoch: LeaseEpoch,
        attempt_index: AttemptIndex,
    ) -> Self {
        Self {
            lease_id,
            lease_epoch,
            attempt_index,
        }
    }
}

#[cfg(test)]
mod rfc_014_validation_tests {
    use super::*;

    fn single(wp: &str) -> ResumeCondition {
        ResumeCondition::Single {
            waitpoint_key: wp.to_owned(),
            matcher: SignalMatcher::ByName("x".to_owned()),
        }
    }

    #[test]
    fn single_passes_validate() {
        assert!(single("wpk:a").validate_composite().is_ok());
    }

    #[test]
    fn allof_empty_members_rejected() {
        let c = ResumeCondition::Composite(CompositeBody::AllOf { members: vec![] });
        let e = c.validate_composite().unwrap_err();
        assert!(e.detail.contains("allof_empty_members"), "{}", e.detail);
    }

    #[test]
    fn count_n_zero_rejected() {
        let c = ResumeCondition::Composite(CompositeBody::Count {
            n: 0,
            count_kind: CountKind::DistinctWaitpoints,
            matcher: None,
            waitpoints: vec!["wpk:a".to_owned()],
        });
        let e = c.validate_composite().unwrap_err();
        assert!(e.detail.contains("count_n_zero"), "{}", e.detail);
    }

    #[test]
    fn count_waitpoints_empty_rejected() {
        let c = ResumeCondition::Composite(CompositeBody::Count {
            n: 1,
            count_kind: CountKind::DistinctSources,
            matcher: None,
            waitpoints: vec![],
        });
        let e = c.validate_composite().unwrap_err();
        assert!(e.detail.contains("count_waitpoints_empty"), "{}", e.detail);
    }

    #[test]
    fn count_exceeds_waitpoint_set_rejected_only_for_distinct_waitpoints() {
        // n=3, only 2 waitpoints, DistinctWaitpoints → reject.
        let c = ResumeCondition::Composite(CompositeBody::Count {
            n: 3,
            count_kind: CountKind::DistinctWaitpoints,
            matcher: None,
            waitpoints: vec!["a".into(), "b".into()],
        });
        let e = c.validate_composite().unwrap_err();
        assert!(e.detail.contains("count_exceeds_waitpoint_set"), "{}", e.detail);

        // Same cardinality, DistinctSignals → allowed (no upper bound).
        let c2 = ResumeCondition::Composite(CompositeBody::Count {
            n: 3,
            count_kind: CountKind::DistinctSignals,
            matcher: None,
            waitpoints: vec!["a".into(), "b".into()],
        });
        assert!(c2.validate_composite().is_ok());
    }

    #[test]
    fn depth_4_accepted_depth_5_rejected() {
        // Build Depth-4: AllOf { AllOf { AllOf { AllOf { Single } } } }
        let leaf = single("wpk:leaf");
        let d4 = ResumeCondition::Composite(CompositeBody::AllOf {
            members: vec![ResumeCondition::Composite(CompositeBody::AllOf {
                members: vec![ResumeCondition::Composite(CompositeBody::AllOf {
                    members: vec![ResumeCondition::Composite(CompositeBody::AllOf {
                        members: vec![leaf.clone()],
                    })],
                })],
            })],
        });
        assert!(d4.validate_composite().is_ok());

        // Depth-5 → reject.
        let d5 = ResumeCondition::Composite(CompositeBody::AllOf {
            members: vec![d4],
        });
        let e = d5.validate_composite().unwrap_err();
        assert!(e.detail.contains("exceeds cap"), "{}", e.detail);
    }
}
