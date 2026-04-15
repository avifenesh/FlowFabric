//! Typed FCALL wrappers for signal delivery and resume-claim functions
//! (lua/signal.lua).

use ff_core::contracts::*;
use ff_core::error::ScriptError;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::types::*;

use crate::result::{FcallResult, FromFcallResult};

// Re-export ExecOpKeys from execution.rs for ff_claim_resumed_execution.
use super::execution::ExecOpKeys;

/// Key context for signal delivery operations.
/// Needs exec keys + index keys + lane (for eligible/suspended/delayed).
pub struct SignalOpKeys<'a> {
    pub ctx: &'a ExecKeyContext,
    pub idx: &'a IndexKeys,
    pub lane_id: &'a LaneId,
}

// ─── ff_deliver_signal ────────────────────────────────────────────────
//
// Lua KEYS (13): exec_core, wp_condition, wp_signals_stream,
//                exec_signals_zset, signal_hash, signal_payload,
//                idem_key, waitpoint_hash, suspension_current,
//                eligible_zset, suspended_zset, delayed_zset,
//                suspension_timeout_zset
// Lua ARGV (17): signal_id, execution_id, waitpoint_id, signal_name,
//                signal_category, source_type, source_identity,
//                payload, payload_encoding, idempotency_key,
//                correlation_id, target_scope, created_at,
//                dedup_ttl_ms, resume_delay_ms, signal_maxlen,
//                max_signals_per_execution

ff_function! {
    pub ff_deliver_signal(args: DeliverSignalArgs) -> DeliverSignalResult {
        keys(k: &SignalOpKeys<'_>) {
            k.ctx.core(),                                              // 1
            k.ctx.waitpoint_condition(&args.waitpoint_id),             // 2
            k.ctx.waitpoint_signals(&args.waitpoint_id),               // 3
            k.ctx.exec_signals(),                                      // 4
            k.ctx.signal(&args.signal_id),                             // 5
            k.ctx.signal_payload(&args.signal_id),                     // 6
            args.idempotency_key.as_ref().filter(|ik| !ik.is_empty()).map(|ik| {
                k.ctx.signal_dedup(&args.waitpoint_id, ik)
            }).unwrap_or_else(|| k.ctx.noop()),                        // 7
            k.ctx.waitpoint(&args.waitpoint_id),                       // 8
            k.ctx.suspension_current(),                                // 9
            k.idx.lane_eligible(k.lane_id),                            // 10
            k.idx.lane_suspended(k.lane_id),                           // 11
            k.idx.lane_delayed(k.lane_id),                             // 12
            k.idx.suspension_timeout(),                                // 13
        }
        argv {
            args.signal_id.to_string(),                                // 1
            args.execution_id.to_string(),                             // 2
            args.waitpoint_id.to_string(),                             // 3
            args.signal_name.clone(),                                  // 4
            args.signal_category.clone(),                              // 5
            args.source_type.clone(),                                  // 6
            args.source_identity.clone(),                              // 7
            args.payload.as_ref()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .unwrap_or_default(),                                  // 8
            args.payload_encoding.clone().unwrap_or_else(|| "json".into()), // 9
            args.idempotency_key.clone().unwrap_or_default(),          // 10
            args.correlation_id.clone().unwrap_or_default(),           // 11
            args.target_scope.clone(),                                 // 12
            args.created_at.map(|t| t.to_string()).unwrap_or_default(), // 13
            args.dedup_ttl_ms.unwrap_or(86_400_000).to_string(),       // 14
            args.resume_delay_ms.unwrap_or(0).to_string(),             // 15
            args.signal_maxlen.unwrap_or(1000).to_string(),            // 16
            args.max_signals_per_execution.unwrap_or(10_000).to_string(), // 17
        }
    }
}

impl FromFcallResult for DeliverSignalResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?;
        // DUPLICATE status: {1, "DUPLICATE", existing_signal_id}
        if r.status == "DUPLICATE" {
            let sid_str = r.field_str(0);
            let sid = SignalId::parse(&sid_str)
                .map_err(|e| ScriptError::Parse(format!("bad signal_id: {e}")))?;
            return Ok(DeliverSignalResult::Duplicate {
                existing_signal_id: sid,
            });
        }
        let r = r.into_success()?;
        // ok(signal_id, effect)
        let sid_str = r.field_str(0);
        let effect = r.field_str(1);
        let sid = SignalId::parse(&sid_str)
            .map_err(|e| ScriptError::Parse(format!("bad signal_id: {e}")))?;
        Ok(DeliverSignalResult::Accepted {
            signal_id: sid,
            effect,
        })
    }
}

// ─── ff_buffer_signal_for_pending_waitpoint ───────────────────────────
//
// Lua KEYS (7): exec_core, wp_condition, wp_signals_stream,
//               exec_signals_zset, signal_hash, signal_payload,
//               idem_key
// Lua ARGV (17): same as ff_deliver_signal (unused fields ignored)

ff_function! {
    pub ff_buffer_signal_for_pending_waitpoint(args: BufferSignalArgs) -> BufferSignalResult {
        keys(k: &SignalOpKeys<'_>) {
            k.ctx.core(),                                              // 1
            k.ctx.waitpoint_condition(&args.waitpoint_id),             // 2
            k.ctx.waitpoint_signals(&args.waitpoint_id),               // 3
            k.ctx.exec_signals(),                                      // 4
            k.ctx.signal(&args.signal_id),                             // 5
            k.ctx.signal_payload(&args.signal_id),                     // 6
            args.idempotency_key.as_ref().filter(|ik| !ik.is_empty()).map(|ik| {
                k.ctx.signal_dedup(&args.waitpoint_id, ik)
            }).unwrap_or_else(|| k.ctx.noop()),                        // 7
        }
        argv {
            args.signal_id.to_string(),                                // 1
            args.execution_id.to_string(),                             // 2
            args.waitpoint_id.to_string(),                             // 3
            args.signal_name.clone(),                                  // 4
            args.signal_category.clone(),                              // 5
            args.source_type.clone(),                                  // 6
            args.source_identity.clone(),                              // 7
            args.payload.as_ref()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .unwrap_or_default(),                                  // 8
            args.payload_encoding.clone().unwrap_or_else(|| "json".into()), // 9
            args.idempotency_key.clone().unwrap_or_default(),          // 10
            String::new(),                                             // 11 correlation_id (not in BufferSignalArgs)
            args.target_scope.clone(),                                 // 12
            String::new(),                                             // 13 created_at
            String::new(),                                             // 14 dedup_ttl_ms
            String::new(),                                             // 15 resume_delay_ms (unused)
            String::new(),                                             // 16 signal_maxlen
            String::new(),                                             // 17 max_signals
        }
    }
}

impl FromFcallResult for BufferSignalResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?;
        // DUPLICATE status: {1, "DUPLICATE", existing_signal_id}
        if r.status == "DUPLICATE" {
            let sid_str = r.field_str(0);
            let sid = SignalId::parse(&sid_str)
                .map_err(|e| ScriptError::Parse(format!("bad signal_id: {e}")))?;
            return Ok(BufferSignalResult::Duplicate {
                existing_signal_id: sid,
            });
        }
        let r = r.into_success()?;
        // ok(signal_id, "buffered_for_pending_waitpoint")
        let sid_str = r.field_str(0);
        let sid = SignalId::parse(&sid_str)
            .map_err(|e| ScriptError::Parse(format!("bad signal_id: {e}")))?;
        Ok(BufferSignalResult::Buffered { signal_id: sid })
    }
}

// ─── ff_claim_resumed_execution ───────────────────────────────────────
//
// Lua KEYS (11): exec_core, claim_grant, eligible_zset, lease_expiry_zset,
//                worker_leases, existing_attempt_hash, lease_current,
//                lease_history, active_index, attempt_timeout_zset,
//                execution_deadline_zset
// Lua ARGV (8): execution_id, worker_id, worker_instance_id, lane,
//               capability_snapshot_hash, lease_id, lease_ttl_ms,
//               remaining_attempt_timeout_ms

ff_function! {
    pub ff_claim_resumed_execution(args: ClaimResumedExecutionArgs) -> ClaimResumedExecutionResult {
        keys(k: &ExecOpKeys<'_>) {
            k.ctx.core(),                                              // 1
            k.ctx.claim_grant(),                                       // 2
            k.idx.lane_eligible(k.lane_id),                            // 3
            k.idx.lease_expiry(),                                      // 4
            k.idx.worker_leases(k.worker_instance_id),                 // 5
            k.ctx.attempt_hash(args.current_attempt_index),            // 6
            k.ctx.lease_current(),                                     // 7
            k.ctx.lease_history(),                                     // 8
            k.idx.lane_active(k.lane_id),                              // 9
            k.idx.attempt_timeout(),                                   // 10
            k.idx.execution_deadline(),                                // 11
        }
        argv {
            args.execution_id.to_string(),                             // 1
            args.worker_id.to_string(),                                // 2
            args.worker_instance_id.to_string(),                       // 3
            args.lane_id.to_string(),                                  // 4
            String::new(),                                             // 5 capability_snapshot_hash
            args.lease_id.to_string(),                                 // 6
            args.lease_ttl_ms.to_string(),                             // 7
            args.remaining_attempt_timeout_ms
                .map(|t| t.to_string())
                .unwrap_or_default(),                                  // 8
        }
    }
}

impl FromFcallResult for ClaimResumedExecutionResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        // ok(lease_id, epoch, expires_at, attempt_id, attempt_index, "resumed")
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

        Ok(ClaimResumedExecutionResult::Claimed(ClaimedResumedExecution {
            execution_id: ExecutionId::parse("").unwrap_or_default(), // filled by caller
            lease_id,
            lease_epoch: LeaseEpoch::new(epoch),
            attempt_index: AttemptIndex::new(attempt_index),
            attempt_id,
            lease_expires_at: TimestampMs::from_millis(expires_at),
        }))
    }
}
