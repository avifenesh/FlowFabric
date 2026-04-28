//! Typed FCALL wrappers for execution lifecycle functions (lua/execution.lua).
//!
//! ## Partial-type pattern (RFC-011 §2.4)
//!
//! Post-RFC-011, `ExecutionId` no longer has `Default`, so parsers cannot
//! construct result structs with a placeholder `execution_id` to be
//! overwritten by the caller. Instead, each `ff_function!` wrapper whose
//! result carries an `execution_id` returns a `*Partial` type that omits
//! the field, with a `.complete(execution_id)` combinator the caller
//! invokes after the FCALL returns (the caller always knows the
//! `execution_id` — it supplied the id as ARGV).
//!
//! `complete` is a total match over the Partial variants, so future
//! result variants that carry an `execution_id` force a compile error
//! in `complete` until the new variant is wired through.

use crate::error::ScriptError;
use ff_core::contracts::*;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::state::{AttemptType, PublicState};
use ff_core::types::*;

use crate::result::{FcallResult, FromFcallResult};

// ─── Partial types (RFC-011 §2.4) ──────────────────────────────────────

/// Partial form of [`ClaimedExecution`] used by the parser path;
/// caller-supplied `execution_id` is attached via [`ClaimExecutionResultPartial::complete`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimedExecutionPartial {
    pub lease_id: LeaseId,
    pub lease_epoch: LeaseEpoch,
    pub attempt_index: AttemptIndex,
    pub attempt_id: AttemptId,
    pub attempt_type: AttemptType,
    pub lease_expires_at: TimestampMs,
}

/// Partial form of [`ClaimExecutionResult`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimExecutionResultPartial {
    Claimed(ClaimedExecutionPartial),
}

impl ClaimExecutionResultPartial {
    /// Attach the caller-supplied `execution_id` and lift to the full
    /// [`ClaimExecutionResult`]. Total match over Partial variants.
    pub fn complete(self, execution_id: ExecutionId) -> ClaimExecutionResult {
        match self {
            // Handle is populated by the backend impl (see
            // `ff_backend_valkey::claim_execution_impl`) after the
            // partial completes — ff-script does not have the
            // `BackendTag` context to mint one here. Stub until
            // the backend overwrites it.
            Self::Claimed(p) => ClaimExecutionResult::Claimed(ClaimedExecution::new(
                execution_id,
                p.lease_id,
                p.lease_epoch,
                p.attempt_index,
                p.attempt_id,
                p.attempt_type,
                p.lease_expires_at,
                ff_core::backend::stub_handle_fresh(),
            )),
        }
    }
}

/// Partial form of [`CompleteExecutionResult`] (omits `execution_id`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompleteExecutionResultPartial {
    Completed { public_state: PublicState },
}

impl CompleteExecutionResultPartial {
    pub fn complete(self, execution_id: ExecutionId) -> CompleteExecutionResult {
        match self {
            Self::Completed { public_state } => CompleteExecutionResult::Completed {
                execution_id,
                public_state,
            },
        }
    }
}

/// Partial form of [`CancelExecutionResult`] (omits `execution_id`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CancelExecutionResultPartial {
    Cancelled { public_state: PublicState },
}

impl CancelExecutionResultPartial {
    pub fn complete(self, execution_id: ExecutionId) -> CancelExecutionResult {
        match self {
            Self::Cancelled { public_state } => CancelExecutionResult::Cancelled {
                execution_id,
                public_state,
            },
        }
    }
}

/// Partial form of [`DelayExecutionResult`] (omits `execution_id`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DelayExecutionResultPartial {
    Delayed { public_state: PublicState },
}

impl DelayExecutionResultPartial {
    pub fn complete(self, execution_id: ExecutionId) -> DelayExecutionResult {
        match self {
            Self::Delayed { public_state } => DelayExecutionResult::Delayed {
                execution_id,
                public_state,
            },
        }
    }
}

/// Partial form of [`MoveToWaitingChildrenResult`] (omits `execution_id`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MoveToWaitingChildrenResultPartial {
    Moved { public_state: PublicState },
}

impl MoveToWaitingChildrenResultPartial {
    pub fn complete(self, execution_id: ExecutionId) -> MoveToWaitingChildrenResult {
        match self {
            Self::Moved { public_state } => MoveToWaitingChildrenResult::Moved {
                execution_id,
                public_state,
            },
        }
    }
}

/// Partial form of [`ExpireExecutionResult`].
///
/// Multi-variant: `Expired` carries `execution_id` (lifted); `AlreadyTerminal`
/// does not. `complete` attaches the id only on `Expired`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExpireExecutionResultPartial {
    Expired,
    AlreadyTerminal,
}

impl ExpireExecutionResultPartial {
    pub fn complete(self, execution_id: ExecutionId) -> ExpireExecutionResult {
        match self {
            Self::Expired => ExpireExecutionResult::Expired { execution_id },
            Self::AlreadyTerminal => ExpireExecutionResult::AlreadyTerminal,
        }
    }
}

/// Bundles ExecKeyContext + IndexKeys + lane-scoped index resolution.
/// Passed as the key context to all execution ff_function! invocations.
pub struct ExecOpKeys<'a> {
    pub ctx: &'a ExecKeyContext,
    pub idx: &'a IndexKeys,
    pub lane_id: &'a LaneId,
    pub worker_instance_id: &'a WorkerInstanceId,
}

// ─── ff_create_execution ───────────────────────────────────────────────
//
// Lua KEYS (8): exec_core, payload, policy, tags,
//               eligible_or_delayed_zset, idem_key,
//               execution_deadline_zset, all_executions_set
// Lua ARGV (13): execution_id, namespace, lane_id, execution_kind,
//                priority, creator_identity, policy_json,
//                input_payload, delay_until, dedup_ttl_ms,
//                tags_json, execution_deadline_at, partition_id

ff_function! {
    pub ff_create_execution(args: CreateExecutionArgs) -> CreateExecutionResult {
        keys(k: &ExecOpKeys<'_>) {
            k.ctx.core(),
            k.ctx.payload(),
            k.ctx.policy(),
            k.ctx.tags(),
            // KEYS[5] = scheduling_zset: eligible OR delayed depending on delay_until.
            // Lua ZADDs to this single key for both paths.
            if args.delay_until.is_some() {
                k.idx.lane_delayed(k.lane_id)
            } else {
                k.idx.lane_eligible(k.lane_id)
            },
            args.idempotency_key.as_ref().filter(|ik| !ik.is_empty()).map(|ik| {
                ff_core::keys::idempotency_key(k.ctx.hash_tag(), args.namespace.as_str(), ik)
            }).unwrap_or_else(|| k.ctx.noop()),
            k.idx.execution_deadline(),
            k.idx.all_executions(),
        }
        argv {
            args.execution_id.to_string(),
            args.namespace.to_string(),
            args.lane_id.to_string(),
            args.execution_kind.clone(),
            args.priority.to_string(),
            args.creator_identity.clone(),
            args.policy.as_ref().map(|p| serde_json::to_string(p).unwrap_or_else(|_| "{}".into())).unwrap_or_else(|| "{}".into()),
            String::from_utf8_lossy(&args.input_payload).into_owned(),
            args.delay_until.map(|t| t.to_string()).unwrap_or_default(),
            args.idempotency_key.as_ref().map(|_| "86400000".to_string()).unwrap_or_default(),
            serde_json::to_string(&args.tags).unwrap_or_else(|_| "{}".into()),
            args.execution_deadline_at.map(|t| t.to_string()).unwrap_or_default(),
            args.partition_id.to_string(),
        }
    }
}

impl FromFcallResult for CreateExecutionResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?;
        // DUPLICATE status: {1, "DUPLICATE", execution_id}
        if r.status == "DUPLICATE" {
            let eid_str = r.field_str(0);
            let eid = ExecutionId::parse(&eid_str)
                .map_err(|e| ScriptError::Parse {
                    fcall: "ff_create_execution".into(),
                    execution_id: None,
                    message: format!("bad execution_id: {e}"),
                })?;
            return Ok(CreateExecutionResult::Duplicate { execution_id: eid });
        }
        let r = r.into_success()?;
        let eid_str = r.field_str(0);
        let ps_str = r.field_str(1);
        let eid = ExecutionId::parse(&eid_str)
            .map_err(|e| ScriptError::Parse {
                fcall: "ff_create_execution".into(),
                execution_id: None,
                message: format!("bad execution_id: {e}"),
            })?;
        let public_state = parse_public_state(&ps_str)?;
        Ok(CreateExecutionResult::Created {
            execution_id: eid,
            public_state,
        })
    }
}

// ─── ff_claim_execution ────────────────────────────────────────────────
//
// Lua KEYS (14): exec_core, claim_grant, eligible_zset, lease_expiry_zset,
//                worker_leases, attempt_hash, attempt_usage, attempt_policy,
//                attempts_zset, lease_current, lease_history, active_index,
//                attempt_timeout_zset, execution_deadline_zset
// Lua ARGV (12): execution_id, worker_id, worker_instance_id, lane,
//                capability_snapshot_hash, lease_id, lease_ttl_ms,
//                renew_before_ms, attempt_id, attempt_policy_json,
//                attempt_timeout_ms, execution_deadline_at

ff_function! {
    pub ff_claim_execution(args: ClaimExecutionArgs) -> ClaimExecutionResultPartial {
        keys(k: &ExecOpKeys<'_>) {
            k.ctx.core(),
            k.ctx.claim_grant(),
            k.idx.lane_eligible(k.lane_id),
            k.idx.lease_expiry(),
            k.idx.worker_leases(k.worker_instance_id),
            k.ctx.attempt_hash(args.expected_attempt_index),
            k.ctx.attempt_usage(args.expected_attempt_index),
            k.ctx.attempt_policy(args.expected_attempt_index),
            k.ctx.attempts(),
            k.ctx.lease_current(),
            k.ctx.lease_history(),
            k.idx.lane_active(k.lane_id),
            k.idx.attempt_timeout(),
            k.idx.execution_deadline(),
        }
        argv {
            args.execution_id.to_string(),
            args.worker_id.to_string(),
            args.worker_instance_id.to_string(),
            args.lane_id.to_string(),
            String::new(),
            args.lease_id.to_string(),
            args.lease_ttl_ms.to_string(),
            (args.lease_ttl_ms * 2 / 3).to_string(),
            args.attempt_id.to_string(),
            args.attempt_policy_json.clone(),
            args.attempt_timeout_ms.map(|t| t.to_string()).unwrap_or_default(),
            args.execution_deadline_at.map(|t| t.to_string()).unwrap_or_default(),
        }
    }
}

impl FromFcallResult for ClaimExecutionResultPartial {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        // ok(lease_id, epoch, expires_at, attempt_id, attempt_index, attempt_type)
        let lease_id = LeaseId::parse(&r.field_str(0))
            .map_err(|e| ScriptError::Parse {
                fcall: "ff_claim_execution_result_partial".into(),
                execution_id: None,
                message: format!("bad lease_id: {e}"),
            })?;
        let epoch = r
            .field_str(1)
            .parse::<u64>()
            .map_err(|e| ScriptError::Parse {
                fcall: "ff_claim_execution_result_partial".into(),
                execution_id: None,
                message: format!("bad epoch: {e}"),
            })?;
        let expires_at = r
            .field_str(2)
            .parse::<i64>()
            .map_err(|e| ScriptError::Parse {
                fcall: "ff_claim_execution_result_partial".into(),
                execution_id: None,
                message: format!("bad expires_at: {e}"),
            })?;
        let attempt_id = AttemptId::parse(&r.field_str(3))
            .map_err(|e| ScriptError::Parse {
                fcall: "ff_claim_execution_result_partial".into(),
                execution_id: None,
                message: format!("bad attempt_id: {e}"),
            })?;
        let attempt_index = r
            .field_str(4)
            .parse::<u32>()
            .map_err(|e| ScriptError::Parse {
                fcall: "ff_claim_execution_result_partial".into(),
                execution_id: None,
                message: format!("bad attempt_index: {e}"),
            })?;
        let attempt_type = parse_attempt_type(&r.field_str(5))?;

        Ok(Self::Claimed(ClaimedExecutionPartial {
            lease_id,
            lease_epoch: LeaseEpoch::new(epoch),
            attempt_index: AttemptIndex::new(attempt_index),
            attempt_id,
            attempt_type,
            lease_expires_at: TimestampMs::from_millis(expires_at),
        }))
    }
}

// ─── ff_complete_execution ─────────────────────────────────────────────
//
// Lua KEYS (12): exec_core, attempt_hash, lease_expiry_zset, worker_leases,
//                terminal_zset, lease_current, lease_history, active_index,
//                stream_meta, result_key, attempt_timeout_zset,
//                execution_deadline_zset
// Lua ARGV (6): execution_id, lease_id, lease_epoch, attempt_id,
//               result_payload, source
//
// RFC #58.5: `fence` is `Option<LeaseFence>`. `None` emits empty strings
// for the triple; the Lua then requires `source == "operator_override"`
// or returns `fence_required`.

ff_function! {
    pub ff_complete_execution(args: CompleteExecutionArgs) -> CompleteExecutionResultPartial {
        keys(k: &ExecOpKeys<'_>) {
            k.ctx.core(),
            k.ctx.attempt_hash(args.attempt_index),
            k.idx.lease_expiry(),
            k.idx.worker_leases(k.worker_instance_id),
            k.idx.lane_terminal(k.lane_id),
            k.ctx.lease_current(),
            k.ctx.lease_history(),
            k.idx.lane_active(k.lane_id),
            k.ctx.stream_meta(args.attempt_index),
            k.ctx.result(),
            k.idx.attempt_timeout(),
            k.idx.execution_deadline(),
        }
        argv {
            args.execution_id.to_string(),
            args.fence.as_ref().map(|f| f.lease_id.to_string()).unwrap_or_default(),
            args.fence.as_ref().map(|f| f.lease_epoch.to_string()).unwrap_or_default(),
            args.fence.as_ref().map(|f| f.attempt_id.to_string()).unwrap_or_default(),
            args.result_payload.as_ref()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .unwrap_or_default(),
            args.source.to_string(),
        }
    }
}

impl FromFcallResult for CompleteExecutionResultPartial {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let _r = FcallResult::parse(raw)?.into_success()?;
        // ok("completed")
        Ok(Self::Completed {
            public_state: PublicState::Completed,
        })
    }
}

// ─── ff_cancel_execution ───────────────────────────────────────────────
//
// Lua KEYS (21): exec_core, attempt_hash, stream_meta, lease_current,
//                lease_history, lease_expiry_zset, worker_leases,
//                suspension_current, waitpoint_hash, wp_condition,
//                suspension_timeout_zset, terminal_zset,
//                attempt_timeout_zset, execution_deadline_zset,
//                eligible_zset, delayed_zset, blocked_deps_zset,
//                blocked_budget_zset, blocked_quota_zset,
//                blocked_route_zset, blocked_operator_zset
// Lua ARGV (5): execution_id, reason, source, lease_id, lease_epoch

// Cancel needs suspension/waitpoint keys that depend on runtime state.
// KEYS[9] and [10] require the waitpoint_id from the suspension record,
// which isn't known until we read suspension:current inside the Lua.
// Phase 1 workaround: pass the suspension_current key as a same-slot
// placeholder. The Lua uses EXISTS checks and will no-op when the key
// type doesn't match HSET expectations (suspension_current is a HASH,
// not a waitpoint hash, but EXISTS returns 0 if the suspension was
// already closed/deleted). Phase 3 must pre-read the waitpoint_id and
// pass the real keys.
ff_function! {
    pub ff_cancel_execution(args: CancelExecutionArgs) -> CancelExecutionResultPartial {
        keys(k: &ExecOpKeys<'_>) {
            k.ctx.core(),                                       // 1
            k.ctx.attempt_hash(AttemptIndex::new(0)),           // 2 placeholder
            k.ctx.stream_meta(AttemptIndex::new(0)),            // 3 placeholder
            k.ctx.lease_current(),                              // 4
            k.ctx.lease_history(),                               // 5
            k.idx.lease_expiry(),                               // 6
            k.idx.worker_leases(k.worker_instance_id),          // 7
            k.ctx.suspension_current(),                         // 8
            k.ctx.suspension_current(),                         // 9 placeholder — real: wp hash (Phase 3)
            k.ctx.suspension_current(),                         // 10 placeholder — real: wp condition (Phase 3)
            k.idx.suspension_timeout(),                         // 11
            k.idx.lane_terminal(k.lane_id),                     // 12
            k.idx.attempt_timeout(),                            // 13
            k.idx.execution_deadline(),                         // 14
            k.idx.lane_eligible(k.lane_id),                     // 15
            k.idx.lane_delayed(k.lane_id),                      // 16
            k.idx.lane_blocked_dependencies(k.lane_id),         // 17
            k.idx.lane_blocked_budget(k.lane_id),               // 18
            k.idx.lane_blocked_quota(k.lane_id),                // 19
            k.idx.lane_blocked_route(k.lane_id),                // 20
            k.idx.lane_blocked_operator(k.lane_id),             // 21
        }
        argv {
            args.execution_id.to_string(),
            args.reason.clone(),
            args.source.to_string(),
            args.lease_id.as_ref().map(|l| l.to_string()).unwrap_or_default(),
            args.lease_epoch.as_ref().map(|e| e.to_string()).unwrap_or_default(),
        }
    }
}

impl FromFcallResult for CancelExecutionResultPartial {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let _r = FcallResult::parse(raw)?.into_success()?;
        // ok("cancelled", cancelled_from_state)
        Ok(Self::Cancelled {
            public_state: PublicState::Cancelled,
        })
    }
}

// ─── ff_delay_execution ────────────────────────────────────────────────
//
// Lua KEYS (9): exec_core, attempt_hash, lease_current, lease_history,
//               lease_expiry_zset, worker_leases, active_index,
//               delayed_zset, attempt_timeout_zset
// Lua ARGV (6): execution_id, lease_id, lease_epoch, attempt_id,
//               delay_until, source
//
// RFC #58.5: `fence` is `Option<LeaseFence>`. See ff_complete_execution.

ff_function! {
    pub ff_delay_execution(args: DelayExecutionArgs) -> DelayExecutionResultPartial {
        keys(k: &ExecOpKeys<'_>) {
            k.ctx.core(),
            k.ctx.attempt_hash(args.attempt_index),
            k.ctx.lease_current(),
            k.ctx.lease_history(),
            k.idx.lease_expiry(),
            k.idx.worker_leases(k.worker_instance_id),
            k.idx.lane_active(k.lane_id),
            k.idx.lane_delayed(k.lane_id),
            k.idx.attempt_timeout(),
        }
        argv {
            args.execution_id.to_string(),
            args.fence.as_ref().map(|f| f.lease_id.to_string()).unwrap_or_default(),
            args.fence.as_ref().map(|f| f.lease_epoch.to_string()).unwrap_or_default(),
            args.fence.as_ref().map(|f| f.attempt_id.to_string()).unwrap_or_default(),
            args.delay_until.to_string(),
            args.source.to_string(),
        }
    }
}

impl FromFcallResult for DelayExecutionResultPartial {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let _r = FcallResult::parse(raw)?.into_success()?;
        // ok(delay_until)
        Ok(Self::Delayed {
            public_state: PublicState::Delayed,
        })
    }
}

// ─── ff_move_to_waiting_children ───────────────────────────────────────
//
// Lua KEYS (9): exec_core, attempt_hash, lease_current, lease_history,
//               lease_expiry_zset, worker_leases, active_index,
//               blocked_deps_zset, attempt_timeout_zset
// Lua ARGV (5): execution_id, lease_id, lease_epoch, attempt_id, source
//
// RFC #58.5: `fence` is `Option<LeaseFence>`. See ff_complete_execution.

ff_function! {
    pub ff_move_to_waiting_children(args: MoveToWaitingChildrenArgs) -> MoveToWaitingChildrenResultPartial {
        keys(k: &ExecOpKeys<'_>) {
            k.ctx.core(),
            k.ctx.attempt_hash(args.attempt_index),
            k.ctx.lease_current(),
            k.ctx.lease_history(),
            k.idx.lease_expiry(),
            k.idx.worker_leases(k.worker_instance_id),
            k.idx.lane_active(k.lane_id),
            k.idx.lane_blocked_dependencies(k.lane_id),
            k.idx.attempt_timeout(),
        }
        argv {
            args.execution_id.to_string(),
            args.fence.as_ref().map(|f| f.lease_id.to_string()).unwrap_or_default(),
            args.fence.as_ref().map(|f| f.lease_epoch.to_string()).unwrap_or_default(),
            args.fence.as_ref().map(|f| f.attempt_id.to_string()).unwrap_or_default(),
            args.source.to_string(),
        }
    }
}

impl FromFcallResult for MoveToWaitingChildrenResultPartial {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let _r = FcallResult::parse(raw)?.into_success()?;
        // ok()
        Ok(Self::Moved {
            public_state: PublicState::WaitingChildren,
        })
    }
}

// ─── ff_fail_execution ─────────────────────────────────────────────────
//
// Lua KEYS (12): exec_core, attempt_hash, lease_expiry_zset, worker_leases,
//                terminal_zset, delayed_zset, lease_current, lease_history,
//                active_index, stream_meta, attempt_timeout_zset,
//                execution_deadline_zset
// Lua ARGV (8): execution_id, lease_id, lease_epoch, attempt_id,
//               failure_reason, failure_category, retry_policy_json, source
//
// RFC #58.5: `fence` is `Option<LeaseFence>`. See ff_complete_execution.

ff_function! {
    pub ff_fail_execution(args: FailExecutionArgs) -> FailExecutionResult {
        keys(k: &ExecOpKeys<'_>) {
            k.ctx.core(),
            k.ctx.attempt_hash(args.attempt_index),
            k.idx.lease_expiry(),
            k.idx.worker_leases(k.worker_instance_id),
            k.idx.lane_terminal(k.lane_id),
            k.idx.lane_delayed(k.lane_id),
            k.ctx.lease_current(),
            k.ctx.lease_history(),
            k.idx.lane_active(k.lane_id),
            k.ctx.stream_meta(args.attempt_index),
            k.idx.attempt_timeout(),
            k.idx.execution_deadline(),
        }
        argv {
            args.execution_id.to_string(),
            args.fence.as_ref().map(|f| f.lease_id.to_string()).unwrap_or_default(),
            args.fence.as_ref().map(|f| f.lease_epoch.to_string()).unwrap_or_default(),
            args.fence.as_ref().map(|f| f.attempt_id.to_string()).unwrap_or_default(),
            args.failure_reason.clone(),
            args.failure_category.clone(),
            args.retry_policy_json.clone(),
            args.source.to_string(),
        }
    }
}

impl FromFcallResult for FailExecutionResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        // ok("retry_scheduled", delay_until) or ok("terminal_failed")
        let sub_status = r.field_str(0);
        match sub_status.as_str() {
            "retry_scheduled" => {
                let delay_str = r.field_str(1);
                let delay_ms: i64 = delay_str
                    .parse()
                    .map_err(|e| ScriptError::Parse {
                        fcall: "ff_fail_execution".into(),
                        execution_id: None,
                        message: format!("bad delay_until: {e}"),
                    })?;
                Ok(FailExecutionResult::RetryScheduled {
                    delay_until: TimestampMs::from_millis(delay_ms),
                    next_attempt_index: AttemptIndex::new(0), // computed by claim_execution
                })
            }
            "terminal_failed" => Ok(FailExecutionResult::TerminalFailed),
            _ => Err(ScriptError::Parse {
                fcall: "ff_fail_execution".into(),
                execution_id: None,
                message: format!(
                "unexpected fail sub-status: {sub_status}"
            ),
            }),
        }
    }
}

// ─── ff_expire_execution ───────────────────────────────────────────────
//
// Lua KEYS (14): exec_core, attempt_hash, stream_meta, lease_current,
//                lease_history, lease_expiry_zset, worker_leases,
//                active_index, terminal_zset, attempt_timeout_zset,
//                execution_deadline_zset, suspended_zset,
//                suspension_timeout_zset, suspension_current
// Lua ARGV (2): execution_id, expire_reason

ff_function! {
    pub ff_expire_execution(args: ExpireExecutionArgs) -> ExpireExecutionResultPartial {
        keys(k: &ExecOpKeys<'_>) {
            k.ctx.core(),
            k.ctx.attempt_hash(AttemptIndex::new(0)),   // placeholder
            k.ctx.stream_meta(AttemptIndex::new(0)),     // placeholder
            k.ctx.lease_current(),
            k.ctx.lease_history(),
            k.idx.lease_expiry(),
            k.idx.worker_leases(k.worker_instance_id),
            k.idx.lane_active(k.lane_id),
            k.idx.lane_terminal(k.lane_id),
            k.idx.attempt_timeout(),
            k.idx.execution_deadline(),
            k.idx.lane_suspended(k.lane_id),
            k.idx.suspension_timeout(),
            k.ctx.suspension_current(),
        }
        argv {
            args.execution_id.to_string(),
            args.expire_reason.clone(),
        }
    }
}

impl FromFcallResult for ExpireExecutionResultPartial {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        // ok("expired", from_phase) or ok("already_terminal") or ok("not_found_cleaned")
        let sub = r.field_str(0);
        match sub.as_str() {
            "already_terminal" | "not_found_cleaned" => Ok(Self::AlreadyTerminal),
            "expired" => Ok(Self::Expired),
            _ => Ok(Self::Expired),
        }
    }
}

// ─── ff_set_execution_tags (issue #58.4) ────────────────────────────────
//
// Variadic-ARGV FCALL: k1, v1, k2, v2, ... Hand-rolled instead of the
// `ff_function!` macro because the macro assumes a fixed ARGV vector;
// tags carry a caller-supplied map.
//
// KEYS (2): exec_core, tags_key
// ARGV (>=2, even): k1, v1, k2, v2, ...

/// Call `ff_set_execution_tags`: write caller-supplied tag fields to
/// the execution's separate tags key. Returns the number of pairs
/// applied.
///
/// Tag keys must match `^[a-z][a-z0-9_]*\.` (the reserved caller
/// namespace, e.g. `cairn.task_id`); a violating key makes the FCALL
/// return `invalid_tag_key` carrying the offending key. Callers should
/// use [`validate_tag_key`] for client-side fast-fail before reaching
/// the FCALL. Empty or odd-length input returns `invalid_input`.
pub async fn ff_set_execution_tags(
    conn: &ferriskey::Client,
    ctx: &ExecKeyContext,
    args: &SetExecutionTagsArgs,
) -> Result<SetExecutionTagsResult, ScriptError> {
    let keys = [ctx.core(), ctx.tags()];
    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();

    // Flatten the sorted map into alternating key/value ARGV. BTreeMap
    // iteration is sorted, so ARGV is deterministic for identical
    // inputs.
    let mut argv: Vec<String> = Vec::with_capacity(args.tags.len() * 2);
    for (k, v) in &args.tags {
        argv.push(k.clone());
        argv.push(v.clone());
    }
    let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();

    let raw = conn
        .fcall::<ferriskey::Value>("ff_set_execution_tags", &key_refs, &argv_refs)
        .await
        .map_err(ScriptError::Valkey)?;
    SetExecutionTagsResult::from_fcall_result(&raw)
}

impl FromFcallResult for SetExecutionTagsResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        let count: u32 = r
            .field_str(0)
            .parse()
            .map_err(|e| ScriptError::Parse {
                fcall: "ff_set_execution_tags".into(),
                execution_id: None,
                message: format!("bad tag count: {e}"),
            })?;
        Ok(SetExecutionTagsResult::Ok { count })
    }
}

/// Client-side fast-fail validator for tag keys. Matches the Lua
/// pattern `^[a-z][a-z0-9_]*%.[^.]` — requires:
///
///   * non-empty;
///   * first char is lowercase ASCII letter;
///   * subsequent chars up to the first `.` are lowercase alnum or `_`;
///   * the first `.` is followed by at least one non-dot character
///     (so `cairn.` and `cairn..x` are rejected — the `<field>` part
///     of `<caller>.<field>` must be non-empty).
///
/// Characters in the suffix after the mandatory non-dot char are not
/// further constrained: `app.sub.field` is legal, matching the Lua
/// pattern. Returns `Err(ScriptError::InvalidTagKey(key))` on
/// rejection so callers match the same variant they'd see from the
/// server-side path.
pub fn validate_tag_key(key: &str) -> Result<(), ScriptError> {
    let mut chars = key.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return Err(ScriptError::InvalidTagKey(key.to_owned())),
    };
    if !first.is_ascii_lowercase() {
        return Err(ScriptError::InvalidTagKey(key.to_owned()));
    }
    let mut saw_dot = false;
    for c in chars.by_ref() {
        if c == '.' {
            saw_dot = true;
            break;
        }
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            return Err(ScriptError::InvalidTagKey(key.to_owned()));
        }
    }
    if !saw_dot {
        return Err(ScriptError::InvalidTagKey(key.to_owned()));
    }
    // Require at least one non-dot character after the first dot.
    match chars.next() {
        Some(c) if c != '.' => Ok(()),
        _ => Err(ScriptError::InvalidTagKey(key.to_owned())),
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────

fn parse_public_state(s: &str) -> Result<PublicState, ScriptError> {
    match s {
        "waiting" => Ok(PublicState::Waiting),
        "delayed" => Ok(PublicState::Delayed),
        "rate_limited" => Ok(PublicState::RateLimited),
        "waiting_children" => Ok(PublicState::WaitingChildren),
        "active" => Ok(PublicState::Active),
        "suspended" => Ok(PublicState::Suspended),
        "resumable" => Ok(PublicState::Resumable),
        "completed" => Ok(PublicState::Completed),
        "failed" => Ok(PublicState::Failed),
        "cancelled" => Ok(PublicState::Cancelled),
        "expired" => Ok(PublicState::Expired),
        "skipped" => Ok(PublicState::Skipped),
        _ => Err(ScriptError::Parse {
            fcall: "parse_public_state".into(),
            execution_id: None,
            message: format!("unknown public_state: {s}"),
        }),
    }
}

fn parse_attempt_type(s: &str) -> Result<AttemptType, ScriptError> {
    match s {
        "initial" => Ok(AttemptType::Initial),
        "retry" => Ok(AttemptType::Retry),
        "reclaim" => Ok(AttemptType::Reclaim),
        "replay" => Ok(AttemptType::Replay),
        "fallback" => Ok(AttemptType::Fallback),
        _ => Err(ScriptError::Parse {
            fcall: "parse_attempt_type".into(),
            execution_id: None,
            message: format!("unknown attempt_type: {s}"),
        }),
    }
}

// ─── Partial-type tests (RFC-011 §2.4 acceptance) ──────────────────────
#[cfg(test)]
mod partial_tests {
    use super::*;
    use ff_core::partition::PartitionConfig;

    fn test_eid() -> ExecutionId {
        ExecutionId::for_flow(&FlowId::new(), &PartitionConfig::default())
    }

    #[test]
    fn claim_partial_complete_attaches_execution_id() {
        let partial = ClaimExecutionResultPartial::Claimed(ClaimedExecutionPartial {
            lease_id: LeaseId::new(),
            lease_epoch: LeaseEpoch::new(1),
            attempt_index: AttemptIndex::new(0),
            attempt_id: AttemptId::new(),
            attempt_type: AttemptType::Initial,
            lease_expires_at: TimestampMs::from_millis(1000),
        });
        let eid = test_eid();
        let full = partial.complete(eid.clone());
        match full {
            ClaimExecutionResult::Claimed(c) => assert_eq!(c.execution_id, eid),
            // `#[non_exhaustive]` — v0.12 PR-4 sealed the enum for
            // forward-compat. Single-variant today.
            _ => unreachable!("ClaimExecutionResult has only `Claimed`"),
        }
    }

    #[test]
    fn complete_partial_complete_attaches_execution_id() {
        let partial = CompleteExecutionResultPartial::Completed {
            public_state: PublicState::Completed,
        };
        let eid = test_eid();
        let full = partial.complete(eid.clone());
        match full {
            CompleteExecutionResult::Completed { execution_id, .. } => {
                assert_eq!(execution_id, eid)
            }
        }
    }

    #[test]
    fn cancel_partial_complete_attaches_execution_id() {
        let partial = CancelExecutionResultPartial::Cancelled {
            public_state: PublicState::Cancelled,
        };
        let eid = test_eid();
        let full = partial.complete(eid.clone());
        match full {
            CancelExecutionResult::Cancelled { execution_id, .. } => assert_eq!(execution_id, eid),
        }
    }

    #[test]
    fn delay_partial_complete_attaches_execution_id() {
        let partial = DelayExecutionResultPartial::Delayed {
            public_state: PublicState::Delayed,
        };
        let eid = test_eid();
        let full = partial.complete(eid.clone());
        match full {
            DelayExecutionResult::Delayed { execution_id, .. } => assert_eq!(execution_id, eid),
        }
    }

    #[test]
    fn move_to_waiting_children_partial_complete_attaches_execution_id() {
        let partial = MoveToWaitingChildrenResultPartial::Moved {
            public_state: PublicState::WaitingChildren,
        };
        let eid = test_eid();
        let full = partial.complete(eid.clone());
        match full {
            MoveToWaitingChildrenResult::Moved { execution_id, .. } => {
                assert_eq!(execution_id, eid)
            }
        }
    }

    #[test]
    fn expire_partial_expired_variant_attaches_execution_id() {
        let partial = ExpireExecutionResultPartial::Expired;
        let eid = test_eid();
        let full = partial.complete(eid.clone());
        match full {
            ExpireExecutionResult::Expired { execution_id } => assert_eq!(execution_id, eid),
            _ => panic!("expected Expired variant"),
        }
    }

    #[test]
    fn expire_partial_already_terminal_variant_ignores_execution_id() {
        // Multi-variant exhaustiveness test: AlreadyTerminal has no
        // execution_id field, so complete() passes it through without
        // attaching. Verifies the variant-mirror pattern.
        let partial = ExpireExecutionResultPartial::AlreadyTerminal;
        let eid = test_eid();
        let full = partial.complete(eid);
        assert!(matches!(full, ExpireExecutionResult::AlreadyTerminal));
    }
}

// ─── Tag-key validation tests (issue #58.4) ────────────────────────────
#[cfg(test)]
mod tag_key_tests {
    use super::*;

    #[test]
    fn validate_accepts_caller_namespaced_key() {
        assert!(validate_tag_key("cairn.task_id").is_ok());
        assert!(validate_tag_key("my_app.run_123").is_ok());
        assert!(validate_tag_key("app2.x.y").is_ok());
    }

    #[test]
    fn validate_rejects_key_without_dot() {
        let err = validate_tag_key("no_dot_here").unwrap_err();
        assert!(matches!(err, ScriptError::InvalidTagKey(k) if k == "no_dot_here"));
    }

    #[test]
    fn validate_rejects_empty_key() {
        let err = validate_tag_key("").unwrap_err();
        assert!(matches!(err, ScriptError::InvalidTagKey(k) if k.is_empty()));
    }

    #[test]
    fn validate_rejects_uppercase_first_char() {
        let err = validate_tag_key("Cairn.task_id").unwrap_err();
        assert!(matches!(err, ScriptError::InvalidTagKey(_)));
    }

    #[test]
    fn validate_rejects_leading_digit() {
        let err = validate_tag_key("1cairn.task_id").unwrap_err();
        assert!(matches!(err, ScriptError::InvalidTagKey(_)));
    }

    #[test]
    fn validate_rejects_dash_in_prefix() {
        let err = validate_tag_key("my-app.run").unwrap_err();
        assert!(matches!(err, ScriptError::InvalidTagKey(_)));
    }

    #[test]
    fn validate_accepts_single_char_prefix() {
        assert!(validate_tag_key("a.x").is_ok());
    }

    #[test]
    fn validate_rejects_trailing_dot() {
        // `cairn.` has the prefix + dot but no field segment — the
        // `<field>` part of `<caller>.<field>` is required.
        let err = validate_tag_key("cairn.").unwrap_err();
        assert!(matches!(err, ScriptError::InvalidTagKey(_)));
    }

    #[test]
    fn validate_rejects_double_dot_after_prefix() {
        // `cairn..x` has an empty field segment immediately after the
        // namespace dot.
        let err = validate_tag_key("cairn..x").unwrap_err();
        assert!(matches!(err, ScriptError::InvalidTagKey(_)));
    }

    #[test]
    fn validate_accepts_dots_in_suffix() {
        // After the mandatory non-dot char post-namespace, further
        // dots are fine — `app.sub.field` is a legal nested tag key.
        assert!(validate_tag_key("app.sub.field").is_ok());
    }

    #[test]
    fn from_code_invalid_tag_key_roundtrip() {
        let err = ScriptError::from_code_with_detail("invalid_tag_key", "BadKey").unwrap();
        match err {
            ScriptError::InvalidTagKey(k) => assert_eq!(k, "BadKey"),
            other => panic!("expected InvalidTagKey, got {other:?}"),
        }
    }
}
