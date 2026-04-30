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

// ─── ff_issue_grant_and_claim (cairn #454 Phase 3d) ────────────────────
//
// Lua KEYS (11): exec_core, claim_grant, eligible_zset, lease_expiry_zset,
//                worker_leases, attempts_zset, lease_current, lease_history,
//                active_index, attempt_timeout_zset, execution_deadline_zset.
//                Attempt hash / usage / policy keys are NOT declared — the
//                Lua computes the index internally (fresh path reads
//                `total_attempt_count`, resume path reads
//                `current_attempt_index`) and builds attempt keys
//                dynamically. All attempt keys share the same `{tag}` hash
//                as `exec_core` so they live in the same cluster slot; not
//                listing them in KEYS is cluster-safe.
// Lua ARGV (11): execution_id, worker_id, worker_instance_id, lane,
//                capability_snapshot_hash, lease_id, lease_ttl_ms,
//                attempt_id, attempt_policy_json, attempt_timeout_ms,
//                execution_deadline_at
//                (`renew_before_ms` is derived in Lua as ttl*2/3.)
//
// Return shape: `ok(lease_id, lease_epoch, attempt_index)` — a
// **unified** tuple regardless of whether the Lua dispatched the
// fresh-claim body or the resume-claim body. Collapsed from the
// richer 6-field `ClaimExecutionResultPartial` because
// `ClaimGrantOutcome` only surfaces the lease identity.

/// Partial form of [`ClaimGrantOutcome`]. The Lua returns `(lease_id,
/// lease_epoch, attempt_index)` — no attempt_id / attempt_type because
/// the caller doesn't branch on them (fresh vs resume is handled
/// transparently inside the FCALL).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimGrantOutcomePartial {
    pub lease_id: LeaseId,
    pub lease_epoch: LeaseEpoch,
    pub attempt_index: AttemptIndex,
}

impl ClaimGrantOutcomePartial {
    pub fn complete(self) -> ClaimGrantOutcome {
        ClaimGrantOutcome::new(self.lease_id, self.lease_epoch, self.attempt_index)
    }
}

ff_function! {
    pub ff_issue_grant_and_claim(args: ClaimExecutionArgs) -> ClaimGrantOutcomePartial {
        keys(k: &crate::functions::execution::ExecOpKeys<'_>) {
            k.ctx.core(),
            k.ctx.claim_grant(),
            k.idx.lane_eligible(k.lane_id),
            k.idx.lease_expiry(),
            k.idx.worker_leases(k.worker_instance_id),
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
            args.attempt_id.to_string(),
            args.attempt_policy_json.clone(),
            args.attempt_timeout_ms.map(|t| t.to_string()).unwrap_or_default(),
            args.execution_deadline_at.map(|t| t.to_string()).unwrap_or_default(),
        }
    }
}

impl FromFcallResult for ClaimGrantOutcomePartial {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        // ok(lease_id, lease_epoch, attempt_index)
        let lease_id = LeaseId::parse(&r.field_str(0))
            .map_err(|e| ScriptError::Parse {
                fcall: "ff_issue_grant_and_claim".into(),
                execution_id: None,
                message: format!("bad lease_id: {e}"),
            })?;
        let epoch = r
            .field_str(1)
            .parse::<u64>()
            .map_err(|e| ScriptError::Parse {
                fcall: "ff_issue_grant_and_claim".into(),
                execution_id: None,
                message: format!("bad epoch: {e}"),
            })?;
        let attempt_index = r
            .field_str(2)
            .parse::<u32>()
            .map_err(|e| ScriptError::Parse {
                fcall: "ff_issue_grant_and_claim".into(),
                execution_id: None,
                message: format!("bad attempt_index: {e}"),
            })?;
        Ok(Self {
            lease_id,
            lease_epoch: LeaseEpoch::new(epoch),
            attempt_index: AttemptIndex::new(attempt_index),
        })
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
