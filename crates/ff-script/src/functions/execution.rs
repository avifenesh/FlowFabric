//! Typed FCALL wrappers for execution lifecycle functions (lua/execution.lua).

use ff_core::contracts::*;
use ff_core::error::ScriptError;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::state::{AttemptType, PublicState};
use ff_core::types::*;

use crate::result::{FcallResult, FromFcallResult};

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
            String::new(),
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
                .map_err(|e| ScriptError::Parse(format!("bad execution_id: {e}")))?;
            return Ok(CreateExecutionResult::Duplicate { execution_id: eid });
        }
        let r = r.into_success()?;
        let eid_str = r.field_str(0);
        let ps_str = r.field_str(1);
        let eid = ExecutionId::parse(&eid_str)
            .map_err(|e| ScriptError::Parse(format!("bad execution_id: {e}")))?;
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
    pub ff_claim_execution(args: ClaimExecutionArgs) -> ClaimExecutionResult {
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

impl FromFcallResult for ClaimExecutionResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        // ok(lease_id, epoch, expires_at, attempt_id, attempt_index, attempt_type)
        let lease_id = LeaseId::parse(&r.field_str(0))
            .map_err(|e| ScriptError::Parse(format!("bad lease_id: {e}")))?;
        let epoch = r.field_str(1).parse::<u64>()
            .map_err(|e| ScriptError::Parse(format!("bad epoch: {e}")))?;
        let expires_at = r.field_str(2).parse::<i64>()
            .map_err(|e| ScriptError::Parse(format!("bad expires_at: {e}")))?;
        let attempt_id = AttemptId::parse(&r.field_str(3))
            .map_err(|e| ScriptError::Parse(format!("bad attempt_id: {e}")))?;
        let attempt_index = r.field_str(4).parse::<u32>()
            .map_err(|e| ScriptError::Parse(format!("bad attempt_index: {e}")))?;
        let attempt_type = parse_attempt_type(&r.field_str(5))?;

        Ok(ClaimExecutionResult::Claimed(ClaimedExecution {
            execution_id: ExecutionId::parse("").unwrap_or_default(), // filled by caller
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
// Lua ARGV (5): execution_id, lease_id, lease_epoch, attempt_id, result_payload

ff_function! {
    pub ff_complete_execution(args: CompleteExecutionArgs) -> CompleteExecutionResult {
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
            args.lease_id.to_string(),
            args.lease_epoch.to_string(),
            args.attempt_id.to_string(),
            args.result_payload.as_ref()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .unwrap_or_default(),
        }
    }
}

impl FromFcallResult for CompleteExecutionResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let _r = FcallResult::parse(raw)?.into_success()?;
        // ok("completed")
        Ok(CompleteExecutionResult::Completed {
            execution_id: ExecutionId::parse("").unwrap_or_default(), // filled by caller
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
    pub ff_cancel_execution(args: CancelExecutionArgs) -> CancelExecutionResult {
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

impl FromFcallResult for CancelExecutionResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let _r = FcallResult::parse(raw)?.into_success()?;
        // ok("cancelled", cancelled_from_state)
        Ok(CancelExecutionResult::Cancelled {
            execution_id: ExecutionId::parse("").unwrap_or_default(), // filled by caller
            public_state: PublicState::Cancelled,
        })
    }
}

// ─── ff_delay_execution ────────────────────────────────────────────────
//
// Lua KEYS (9): exec_core, attempt_hash, lease_current, lease_history,
//               lease_expiry_zset, worker_leases, active_index,
//               delayed_zset, attempt_timeout_zset
// Lua ARGV (5): execution_id, lease_id, lease_epoch, attempt_id, delay_until

ff_function! {
    pub ff_delay_execution(args: DelayExecutionArgs) -> DelayExecutionResult {
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
            args.lease_id.to_string(),
            args.lease_epoch.to_string(),
            args.attempt_id.to_string(),
            args.delay_until.to_string(),
        }
    }
}

impl FromFcallResult for DelayExecutionResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let _r = FcallResult::parse(raw)?.into_success()?;
        // ok(delay_until)
        Ok(DelayExecutionResult::Delayed {
            execution_id: ExecutionId::parse("").unwrap_or_default(), // filled by caller
            public_state: PublicState::Delayed,
        })
    }
}

// ─── ff_move_to_waiting_children ───────────────────────────────────────
//
// Lua KEYS (9): exec_core, attempt_hash, lease_current, lease_history,
//               lease_expiry_zset, worker_leases, active_index,
//               blocked_deps_zset, attempt_timeout_zset
// Lua ARGV (4): execution_id, lease_id, lease_epoch, attempt_id

ff_function! {
    pub ff_move_to_waiting_children(args: MoveToWaitingChildrenArgs) -> MoveToWaitingChildrenResult {
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
            args.lease_id.to_string(),
            args.lease_epoch.to_string(),
            args.attempt_id.to_string(),
        }
    }
}

impl FromFcallResult for MoveToWaitingChildrenResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let _r = FcallResult::parse(raw)?.into_success()?;
        // ok()
        Ok(MoveToWaitingChildrenResult::Moved {
            execution_id: ExecutionId::parse("").unwrap_or_default(), // filled by caller
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
// Lua ARGV (7): execution_id, lease_id, lease_epoch, attempt_id,
//               failure_reason, failure_category, retry_policy_json

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
            args.lease_id.to_string(),
            args.lease_epoch.to_string(),
            args.attempt_id.to_string(),
            args.failure_reason.clone(),
            args.failure_category.clone(),
            args.retry_policy_json.clone(),
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
                    .map_err(|e| ScriptError::Parse(format!("bad delay_until: {e}")))?;
                Ok(FailExecutionResult::RetryScheduled {
                    delay_until: TimestampMs::from_millis(delay_ms),
                    next_attempt_index: AttemptIndex::new(0), // computed by claim_execution
                })
            }
            "terminal_failed" => Ok(FailExecutionResult::TerminalFailed),
            _ => Err(ScriptError::Parse(format!(
                "unexpected fail sub-status: {sub_status}"
            ))),
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
    pub ff_expire_execution(args: ExpireExecutionArgs) -> ExpireExecutionResult {
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

impl FromFcallResult for ExpireExecutionResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        // ok("expired", from_phase) or ok("already_terminal") or ok("not_found_cleaned")
        let sub = r.field_str(0);
        match sub.as_str() {
            "already_terminal" | "not_found_cleaned" => Ok(ExpireExecutionResult::AlreadyTerminal),
            "expired" => Ok(ExpireExecutionResult::Expired {
                execution_id: ExecutionId::parse("").unwrap_or_default(),
            }),
            _ => Ok(ExpireExecutionResult::Expired {
                execution_id: ExecutionId::parse("").unwrap_or_default(),
            }),
        }
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
        "completed" => Ok(PublicState::Completed),
        "failed" => Ok(PublicState::Failed),
        "cancelled" => Ok(PublicState::Cancelled),
        "expired" => Ok(PublicState::Expired),
        "skipped" => Ok(PublicState::Skipped),
        _ => Err(ScriptError::Parse(format!("unknown public_state: {s}"))),
    }
}

fn parse_attempt_type(s: &str) -> Result<AttemptType, ScriptError> {
    match s {
        "initial" => Ok(AttemptType::Initial),
        "retry" => Ok(AttemptType::Retry),
        "reclaim" => Ok(AttemptType::Reclaim),
        "replay" => Ok(AttemptType::Replay),
        "fallback" => Ok(AttemptType::Fallback),
        _ => Err(ScriptError::Parse(format!("unknown attempt_type: {s}"))),
    }
}
