//! Typed FCALL wrappers for scheduling functions (lua/scheduling.lua).
//!
//! See `execution.rs` module-level rustdoc for the Partial-type pattern
//! rationale (RFC-011 §2.4).

use ff_core::contracts::*;
use crate::error::ScriptError;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::types::*;

use crate::result::{FcallResult, FromFcallResult};

/// Partial form of [`ChangePriorityResult`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangePriorityResultPartial {
    Changed,
}

impl ChangePriorityResultPartial {
    pub fn complete(self, execution_id: ExecutionId) -> ChangePriorityResult {
        match self {
            Self::Changed => ChangePriorityResult::Changed { execution_id },
        }
    }
}

/// Key context for scheduling operations that need exec_core + index keys.
pub struct SchedOpKeys<'a> {
    pub ctx: &'a ExecKeyContext,
    pub idx: &'a IndexKeys,
    pub lane_id: &'a LaneId,
}

// ─── ff_issue_claim_grant ──────────────────────────────────────────────
//
// Lua KEYS (3): exec_core, claim_grant_key, eligible_zset
// Lua ARGV (9): execution_id, worker_id, worker_instance_id,
//               lane_id, capability_hash, grant_ttl_ms,
//               route_snapshot_json, admission_summary,
//               worker_capabilities_csv

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
            // BTreeSet iterates in sorted order → stable CSV for Lua match
            args.worker_capabilities.iter().cloned().collect::<Vec<_>>().join(","),
        }
    }
}

impl FromFcallResult for IssueClaimGrantResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        // ok(execution_id)
        let eid_str = r.field_str(0);
        let eid = ExecutionId::parse(&eid_str)
            .map_err(|e| ScriptError::Parse {
                fcall: "ff_issue_claim_grant".into(),
                execution_id: None,
                message: format!("bad execution_id: {e}"),
            })?;
        Ok(IssueClaimGrantResult::Granted { execution_id: eid })
    }
}

// ─── ff_change_priority ────────────────────────────────────────────────
//
// Lua KEYS (2): exec_core, eligible_zset
// Lua ARGV (2): execution_id, new_priority

ff_function! {
    pub ff_change_priority(args: ChangePriorityArgs) -> ChangePriorityResultPartial {
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

impl FromFcallResult for ChangePriorityResultPartial {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let _r = FcallResult::parse(raw)?.into_success()?;
        // ok(old_priority, new_priority)
        Ok(Self::Changed)
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

// ─── Partial-type tests (RFC-011 §2.4 acceptance) ──────────────────────
#[cfg(test)]
mod partial_tests {
    use super::*;
    use ff_core::partition::PartitionConfig;

    #[test]
    fn change_priority_partial_complete_attaches_execution_id() {
        let partial = ChangePriorityResultPartial::Changed;
        let eid = ExecutionId::for_flow(&FlowId::new(), &PartitionConfig::default());
        let full = partial.complete(eid.clone());
        match full {
            ChangePriorityResult::Changed { execution_id } => assert_eq!(execution_id, eid),
        }
    }
}
