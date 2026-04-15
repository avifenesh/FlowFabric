//! Typed FCALL wrappers for scheduling functions (lua/scheduling.lua).

use ff_core::contracts::*;
use ff_core::error::ScriptError;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::types::*;

use crate::result::{FcallResult, FromFcallResult};

/// Key context for scheduling operations that need exec_core + index keys.
pub struct SchedOpKeys<'a> {
    pub ctx: &'a ExecKeyContext,
    pub idx: &'a IndexKeys,
    pub lane_id: &'a LaneId,
}

// ─── ff_issue_claim_grant ──────────────────────────────────────────────
//
// Lua KEYS (3): exec_core, claim_grant_key, eligible_zset
// Lua ARGV (8): execution_id, worker_id, worker_instance_id,
//               lane_id, capability_hash, grant_ttl_ms,
//               route_snapshot_json, admission_summary

ff_function! {
    pub ff_issue_claim_grant(args: IssueClaimGrantArgs) -> IssueClaimGrantResult {
        keys(k: &SchedOpKeys<'_>) {
            k.ctx.core(),
            k.ctx.claim_grant(),
            k.idx.lane_eligible(k.lane_id),
        }
        argv {
            args.execution_id.to_string(),
            args.worker_id.to_string(),
            args.worker_instance_id.to_string(),
            args.lane_id.to_string(),
            args.capability_hash.clone().unwrap_or_default(),
            args.grant_ttl_ms.to_string(),
            args.route_snapshot_json.clone().unwrap_or_default(),
            args.admission_summary.clone().unwrap_or_default(),
        }
    }
}

impl FromFcallResult for IssueClaimGrantResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        // ok(execution_id)
        let eid_str = r.field_str(0);
        let eid = ExecutionId::parse(&eid_str)
            .map_err(|e| ScriptError::Parse(format!("bad execution_id: {e}")))?;
        Ok(IssueClaimGrantResult::Granted { execution_id: eid })
    }
}

// ─── ff_change_priority ────────────────────────────────────────────────
//
// Lua KEYS (2): exec_core, eligible_zset
// Lua ARGV (2): execution_id, new_priority

ff_function! {
    pub ff_change_priority(args: ChangePriorityArgs) -> ChangePriorityResult {
        keys(k: &SchedOpKeys<'_>) {
            k.ctx.core(),
            k.idx.lane_eligible(k.lane_id),
        }
        argv {
            args.execution_id.to_string(),
            args.new_priority.to_string(),
        }
    }
}

impl FromFcallResult for ChangePriorityResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let _r = FcallResult::parse(raw)?.into_success()?;
        // ok(old_priority, new_priority)
        Ok(ChangePriorityResult::Changed {
            execution_id: ExecutionId::parse("").unwrap_or_default(), // filled by caller
        })
    }
}

// ─── ff_update_progress ────────────────────────────────────────────────
//
// Lua KEYS (1): exec_core
// Lua ARGV (5): execution_id, lease_id, lease_epoch, progress_pct, progress_message

ff_function! {
    pub ff_update_progress(args: UpdateProgressArgs) -> UpdateProgressResult {
        keys(k: &SchedOpKeys<'_>) {
            k.ctx.core(),
        }
        argv {
            args.execution_id.to_string(),
            args.lease_id.to_string(),
            args.lease_epoch.to_string(),
            args.progress_pct.map(|p| p.to_string()).unwrap_or_default(),
            args.progress_message.clone().unwrap_or_default(),
        }
    }
}

impl FromFcallResult for UpdateProgressResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let _r = FcallResult::parse(raw)?.into_success()?;
        // ok()
        Ok(UpdateProgressResult::Updated)
    }
}
