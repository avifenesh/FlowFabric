//! Typed FCALL wrappers for flow coordination functions (lua/flow.lua).

use ff_core::contracts::*;
use ff_core::error::ScriptError;
use ff_core::keys::{ExecKeyContext, FlowKeyContext, IndexKeys};
use ff_core::state::PublicState;

use crate::result::{FcallResult, FromFcallResult};

/// Key context for flow-structural operations on {fp:N}.
pub struct FlowStructOpKeys<'a> {
    pub fctx: &'a FlowKeyContext,
}

/// Key context for child-local dependency operations on {p:N}.
pub struct DepOpKeys<'a> {
    pub ctx: &'a ExecKeyContext,
    pub idx: &'a IndexKeys,
    pub lane_id: &'a ff_core::types::LaneId,
}

// ─── ff_create_flow ──────────────────────────────────────────────────
// KEYS (2): flow_core, members_set
// ARGV (4): flow_id, flow_kind, namespace, now_ms

ff_function! {
    pub ff_create_flow(args: CreateFlowArgs) -> CreateFlowResult {
        keys(k: &FlowStructOpKeys<'_>) {
            k.fctx.core(),
            k.fctx.members(),
        }
        argv {
            args.flow_id.to_string(),
            args.flow_kind.clone(),
            args.namespace.to_string(),
            args.now.to_string(),
        }
    }
}

impl FromFcallResult for CreateFlowResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        let fid_str = r.field_str(0);
        let fid = ff_core::types::FlowId::parse(&fid_str)
            .map_err(|e| ScriptError::Parse(format!("bad flow_id: {e}")))?;
        match r.status.as_str() {
            "OK" => Ok(CreateFlowResult::Created { flow_id: fid }),
            "ALREADY_SATISFIED" => Ok(CreateFlowResult::AlreadySatisfied { flow_id: fid }),
            _ => Err(ScriptError::Parse(format!("unexpected status: {}", r.status))),
        }
    }
}

// ─── ff_add_execution_to_flow ────────────────────────────────────────
// KEYS (2): flow_core, members_set
// ARGV (3): flow_id, execution_id, now_ms

ff_function! {
    pub ff_add_execution_to_flow(args: AddExecutionToFlowArgs) -> AddExecutionToFlowResult {
        keys(k: &FlowStructOpKeys<'_>) {
            k.fctx.core(),
            k.fctx.members(),
        }
        argv {
            args.flow_id.to_string(),
            args.execution_id.to_string(),
            args.now.to_string(),
        }
    }
}

impl FromFcallResult for AddExecutionToFlowResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        let eid_str = r.field_str(0);
        let nc_str = r.field_str(1);
        match r.status.as_str() {
            "ALREADY_SATISFIED" => {
                let eid = ff_core::types::ExecutionId::parse(&eid_str)
                    .map_err(|e| ScriptError::Parse(format!("bad execution_id: {e}")))?;
                let nc: u32 = nc_str.parse().unwrap_or(0);
                Ok(AddExecutionToFlowResult::AlreadyMember {
                    execution_id: eid,
                    node_count: nc,
                })
            }
            "OK" => {
                let eid = ff_core::types::ExecutionId::parse(&eid_str)
                    .map_err(|e| ScriptError::Parse(format!("bad execution_id: {e}")))?;
                let nc: u32 = nc_str.parse().unwrap_or(0);
                Ok(AddExecutionToFlowResult::Added {
                    execution_id: eid,
                    new_node_count: nc,
                })
            }
            _ => Err(ScriptError::Parse(format!("unexpected status: {}", r.status))),
        }
    }
}

// ─── ff_cancel_flow ──────────────────────────────────────────────────
// KEYS (2): flow_core, members_set
// ARGV (4): flow_id, reason, cancellation_policy, now_ms

ff_function! {
    pub ff_cancel_flow(args: CancelFlowArgs) -> CancelFlowResult {
        keys(k: &FlowStructOpKeys<'_>) {
            k.fctx.core(),
            k.fctx.members(),
        }
        argv {
            args.flow_id.to_string(),
            args.reason.clone(),
            args.cancellation_policy.clone(),
            args.now.to_string(),
        }
    }
}

impl FromFcallResult for CancelFlowResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        let policy = r.field_str(0);
        let mut members = Vec::new();
        let mut i = 1;
        loop {
            let s = r.field_str(i);
            if s.is_empty() {
                break;
            }
            members.push(s);
            i += 1;
        }
        Ok(CancelFlowResult::Cancelled {
            cancellation_policy: policy,
            member_execution_ids: members,
        })
    }
}

// ─── ff_evaluate_flow_eligibility ─────────────────────────────────────
// KEYS (2): exec_core, deps_meta
// ARGV (0)

ff_function! {
    #[allow(unused_variables)]
    pub ff_evaluate_flow_eligibility(args: EvaluateFlowEligibilityArgs) -> EvaluateFlowEligibilityResult {
        keys(k: &DepOpKeys<'_>) {
            k.ctx.core(),
            k.ctx.deps_meta(),
        }
        argv {
        }
    }
}

impl FromFcallResult for EvaluateFlowEligibilityResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        Ok(EvaluateFlowEligibilityResult::Status {
            status: r.field_str(0),
        })
    }
}

// ─── ff_apply_dependency_to_child ─────────────────────────────────────
// KEYS (6): exec_core, deps_meta, unresolved_set, dep_hash,
//           eligible_zset, blocked_deps_zset
// ARGV (7): flow_id, edge_id, upstream_eid, graph_revision,
//           dependency_kind, data_passing_ref, now_ms

ff_function! {
    pub ff_apply_dependency_to_child(args: ApplyDependencyToChildArgs) -> ApplyDependencyToChildResult {
        keys(k: &DepOpKeys<'_>) {
            k.ctx.core(),
            k.ctx.deps_meta(),
            k.ctx.deps_unresolved(),
            k.ctx.dep_edge(&args.edge_id),
            k.idx.lane_eligible(k.lane_id),
            k.idx.lane_blocked_dependencies(k.lane_id),
        }
        argv {
            args.flow_id.to_string(),
            args.edge_id.to_string(),
            args.upstream_execution_id.to_string(),
            args.graph_revision.to_string(),
            args.dependency_kind.clone(),
            args.data_passing_ref.clone().unwrap_or_default(),
            args.now.to_string(),
        }
    }
}

impl FromFcallResult for ApplyDependencyToChildResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        let sub = r.field_str(0);
        if sub == "already_applied" {
            Ok(ApplyDependencyToChildResult::AlreadyApplied)
        } else {
            let count: u32 = sub.parse().unwrap_or(0);
            Ok(ApplyDependencyToChildResult::Applied {
                unsatisfied_count: count,
            })
        }
    }
}

// ─── ff_resolve_dependency ────────────────────────────────────────────
// KEYS (9): exec_core, deps_meta, unresolved_set, dep_hash,
//           eligible_zset, terminal_zset, blocked_deps_zset,
//           attempt_hash, stream_meta
// ARGV (3): edge_id, upstream_outcome, now_ms

ff_function! {
    pub ff_resolve_dependency(args: ResolveDependencyArgs) -> ResolveDependencyResult {
        keys(k: &DepOpKeys<'_>) {
            k.ctx.core(),
            k.ctx.deps_meta(),
            k.ctx.deps_unresolved(),
            k.ctx.dep_edge(&args.edge_id),
            k.idx.lane_eligible(k.lane_id),
            k.idx.lane_terminal(k.lane_id),
            k.idx.lane_blocked_dependencies(k.lane_id),
            k.ctx.attempt_hash(ff_core::types::AttemptIndex::new(0)), // placeholder
            k.ctx.stream_meta(ff_core::types::AttemptIndex::new(0)),  // placeholder
        }
        argv {
            args.edge_id.to_string(),
            args.upstream_outcome.clone(),
            args.now.to_string(),
        }
    }
}

impl FromFcallResult for ResolveDependencyResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        match r.field_str(0).as_str() {
            "satisfied" => Ok(ResolveDependencyResult::Satisfied),
            "impossible" => Ok(ResolveDependencyResult::Impossible),
            "already_resolved" => Ok(ResolveDependencyResult::AlreadyResolved),
            other => Err(ScriptError::Parse(format!("unknown resolve status: {other}"))),
        }
    }
}

// ─── ff_promote_blocked_to_eligible ───────────────────────────────────
// KEYS (5): exec_core, blocked_deps_zset, eligible_zset, deps_meta,
//           deps_unresolved
// ARGV (2): execution_id, now_ms

ff_function! {
    pub ff_promote_blocked_to_eligible(args: PromoteBlockedToEligibleArgs) -> PromoteBlockedToEligibleResult {
        keys(k: &DepOpKeys<'_>) {
            k.ctx.core(),
            k.idx.lane_blocked_dependencies(k.lane_id),
            k.idx.lane_eligible(k.lane_id),
            k.ctx.deps_meta(),
            k.ctx.deps_unresolved(),
        }
        argv {
            args.execution_id.to_string(),
            args.now.to_string(),
        }
    }
}

impl FromFcallResult for PromoteBlockedToEligibleResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let _r = FcallResult::parse(raw)?.into_success()?;
        Ok(PromoteBlockedToEligibleResult::Promoted)
    }
}

// ─── ff_replay_execution ──────────────────────────────────────────────
// KEYS (4+N): exec_core, terminal_zset, eligible_zset, lease_history,
//             [blocked_deps_zset, deps_meta, deps_unresolved, dep_edge_0..N]
// ARGV (2+N): execution_id, now_ms, [edge_id_0..N]
//
// NOTE: Variable KEYS/ARGV. The ff_function! macro generates a fixed-size
// Vec, but this function needs dynamic N edges. For skipped flow member
// replay, use the manual FCALL path instead of this wrapper.
// This wrapper handles the common non-flow replay case (base 4 KEYS).

ff_function! {
    pub ff_replay_execution(args: ReplayExecutionArgs) -> ReplayExecutionResult {
        keys(k: &DepOpKeys<'_>) {
            k.ctx.core(),
            k.idx.lane_terminal(k.lane_id),
            k.idx.lane_eligible(k.lane_id),
            k.ctx.lease_history(),
        }
        argv {
            args.execution_id.to_string(),
            args.now.to_string(),
        }
    }
}

impl FromFcallResult for ReplayExecutionResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        // ok("0") for normal, ok(N) for skipped flow member
        let unsatisfied = r.field_str(0);
        let ps = if unsatisfied == "0" {
            PublicState::Waiting
        } else {
            PublicState::WaitingChildren
        };
        Ok(ReplayExecutionResult::Replayed { public_state: ps })
    }
}

// ─── ff_stage_dependency_edge ─────────────────────────────────────────
// KEYS (6): flow_core, members_set, edge_hash, out_adj_set, in_adj_set,
//           grant_hash
// ARGV (8): flow_id, edge_id, upstream_eid, downstream_eid,
//           dependency_kind, data_passing_ref, expected_graph_revision,
//           now_ms

ff_function! {
    pub ff_stage_dependency_edge(args: StageDependencyEdgeArgs) -> StageDependencyEdgeResult {
        keys(k: &FlowStructOpKeys<'_>) {
            k.fctx.core(),
            k.fctx.members(),
            k.fctx.edge(&args.edge_id),
            k.fctx.outgoing(&args.upstream_execution_id),
            k.fctx.incoming(&args.downstream_execution_id),
            k.fctx.grant(&args.edge_id.to_string()),
        }
        argv {
            args.flow_id.to_string(),
            args.edge_id.to_string(),
            args.upstream_execution_id.to_string(),
            args.downstream_execution_id.to_string(),
            args.dependency_kind.clone(),
            args.data_passing_ref.clone().unwrap_or_default(),
            args.expected_graph_revision.to_string(),
            args.now.to_string(),
        }
    }
}

impl FromFcallResult for StageDependencyEdgeResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        let eid = ff_core::types::EdgeId::parse(&r.field_str(0))
            .map_err(|e| ScriptError::Parse(format!("bad edge_id: {e}")))?;
        let rev: u64 = r.field_str(1).parse()
            .map_err(|e| ScriptError::Parse(format!("bad graph_revision: {e}")))?;
        Ok(StageDependencyEdgeResult::Staged {
            edge_id: eid,
            new_graph_revision: rev,
        })
    }
}
