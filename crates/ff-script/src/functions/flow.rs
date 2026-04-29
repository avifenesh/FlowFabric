//! Typed FCALL wrappers for flow coordination functions (lua/flow.lua).

use ff_core::contracts::*;
use crate::error::ScriptError;
use ff_core::keys::{ExecKeyContext, FlowIndexKeys, FlowKeyContext, IndexKeys};
use ff_core::state::PublicState;

use crate::result::{FcallResult, FromFcallResult};

/// Key context for flow-structural operations on {fp:N}.
pub struct FlowStructOpKeys<'a> {
    pub fctx: &'a FlowKeyContext,
    pub fidx: &'a FlowIndexKeys,
}

/// Key context for [`ff_add_execution_to_flow`]. Extends
/// [`FlowStructOpKeys`] with the member execution's `exec_core`
/// key — Lua expects KEYS(4) with `exec_core` at position 4 to
/// stamp the flow back-pointer atomically (RFC-011 §7.3). Using
/// the bare `FlowStructOpKeys` here wires only KEYS(3) and the
/// function fails at runtime with `redis.call("EXISTS", nil)`.
pub struct AddExecutionToFlowKeys<'a> {
    pub fctx: &'a FlowKeyContext,
    pub fidx: &'a FlowIndexKeys,
    pub exec_ctx: &'a ExecKeyContext,
}

/// Key context for child-local dependency operations on {p:N}.
///
/// `flow_ctx` is required for dependency ops (every dependency edge
/// belongs to a flow); it supplies the `edgegroup` key used by
/// RFC-016 Stage A. Post-RFC-011 the flow and exec partitions share
/// the `{fp:N}` hash tag so the FCALL is still single-slot.
pub struct DepOpKeys<'a> {
    pub ctx: &'a ExecKeyContext,
    pub idx: &'a IndexKeys,
    pub lane_id: &'a ff_core::types::LaneId,
    pub flow_ctx: &'a FlowKeyContext,
    pub downstream_eid: &'a ff_core::types::ExecutionId,
}

/// Extended key context for [`ff_resolve_dependency`], which needs
/// access to the upstream execution's result key for server-side
/// `data_passing_ref` resolution (Batch C item 3). Separate from
/// [`DepOpKeys`] so the other dependency wrappers —
/// `ff_apply_dependency_to_child`, `ff_evaluate_flow_eligibility`,
/// `ff_promote_blocked_to_eligible`, `ff_replay_execution` — don't
/// have to carry an upstream context they never use.
///
/// Upstream and downstream are co-located on the same `{fp:N}` slot
/// via flow membership (RFC-011 §7.3), so `upstream_ctx` builds the
/// upstream key on the child's partition.
pub struct ResolveDependencyKeys<'a> {
    pub ctx: &'a ExecKeyContext,
    pub idx: &'a IndexKeys,
    pub lane_id: &'a ff_core::types::LaneId,
    pub upstream_ctx: &'a ExecKeyContext,
    pub flow_ctx: &'a FlowKeyContext,
    pub flow_idx: &'a FlowIndexKeys,
    pub downstream_eid: &'a ff_core::types::ExecutionId,
    /// Child's current attempt index — needed to build the correct
    /// `attempt_hash` + `stream_meta` KEYS. PR-7b Step 0 lift: prior
    /// to the trait migration the wrapper hardcoded
    /// `AttemptIndex::new(0)` as a placeholder because the only caller
    /// was a scanner path that had other guards; the trait entry-
    /// point is hot-path (post-completion cascade) so it must pin
    /// the right attempt.
    pub current_attempt_index: ff_core::types::AttemptIndex,
}

// ─── ff_create_flow ──────────────────────────────────────────────────
// KEYS (3): flow_core, members_set, flow_index
// ARGV (4): flow_id, flow_kind, namespace, now_ms

ff_function! {
    pub ff_create_flow(args: CreateFlowArgs) -> CreateFlowResult {
        keys(k: &FlowStructOpKeys<'_>) {
            k.fctx.core(),
            k.fctx.members(),
            k.fidx.flow_index(),
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
            .map_err(|e| ScriptError::Parse {
                fcall: "ff_create_flow".into(),
                execution_id: None,
                message: format!("bad flow_id: {e}"),
            })?;
        match r.status.as_str() {
            "OK" => Ok(CreateFlowResult::Created { flow_id: fid }),
            "ALREADY_SATISFIED" => Ok(CreateFlowResult::AlreadySatisfied { flow_id: fid }),
            _ => Err(ScriptError::Parse {
                fcall: "ff_create_flow".into(),
                execution_id: None,
                message: format!("unexpected status: {}", r.status),
            }),
        }
    }
}

// ─── ff_add_execution_to_flow ────────────────────────────────────────
// KEYS (3): flow_core, members_set, flow_index
// ARGV (3): flow_id, execution_id, now_ms

ff_function! {
    pub ff_add_execution_to_flow(args: AddExecutionToFlowArgs) -> AddExecutionToFlowResult {
        keys(k: &AddExecutionToFlowKeys<'_>) {
            k.fctx.core(),
            k.fctx.members(),
            k.fidx.flow_index(),
            k.exec_ctx.core(),
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
                    .map_err(|e| ScriptError::Parse {
                        fcall: "ff_add_execution_to_flow".into(),
                        execution_id: None,
                        message: format!("bad execution_id: {e}"),
                    })?;
                let nc: u32 = nc_str.parse().unwrap_or(0);
                Ok(AddExecutionToFlowResult::AlreadyMember {
                    execution_id: eid,
                    node_count: nc,
                })
            }
            "OK" => {
                let eid = ff_core::types::ExecutionId::parse(&eid_str)
                    .map_err(|e| ScriptError::Parse {
                        fcall: "ff_add_execution_to_flow".into(),
                        execution_id: None,
                        message: format!("bad execution_id: {e}"),
                    })?;
                let nc: u32 = nc_str.parse().unwrap_or(0);
                Ok(AddExecutionToFlowResult::Added {
                    execution_id: eid,
                    new_node_count: nc,
                })
            }
            _ => Err(ScriptError::Parse {
                fcall: "ff_add_execution_to_flow".into(),
                execution_id: None,
                message: format!("unexpected status: {}", r.status),
            }),
        }
    }
}

// ─── ff_cancel_flow ──────────────────────────────────────────────────
// KEYS (5): flow_core, members_set, flow_index, pending_cancels, cancel_backlog
// ARGV (5): flow_id, reason, cancellation_policy, now_ms, grace_ms
//
// pending_cancels + cancel_backlog are populated only on policy=cancel_all
// with members > 0. The cancel_reconciler scanner drains any entries
// live dispatch leaves behind. Passing grace_ms as "" accepts the Lua
// default (30s).

ff_function! {
    pub ff_cancel_flow(args: CancelFlowArgs) -> CancelFlowResult {
        keys(k: &FlowStructOpKeys<'_>) {
            k.fctx.core(),
            k.fctx.members(),
            k.fidx.flow_index(),
            k.fctx.pending_cancels(),
            k.fidx.cancel_backlog(),
        }
        argv {
            args.flow_id.to_string(),
            args.reason.clone(),
            args.cancellation_policy.clone(),
            args.now.to_string(),
            String::new(),
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
// KEYS (8): exec_core, deps_meta, unresolved_set, dep_hash,
//           eligible_zset, blocked_deps_zset, deps_all_edges, edgegroup
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
            k.ctx.deps_all_edges(),
            k.flow_ctx.edgegroup(k.downstream_eid),
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
// KEYS (14): exec_core, deps_meta, unresolved_set, dep_hash,
//            eligible_zset, terminal_zset, blocked_deps_zset,
//            attempt_hash, stream_meta, downstream_payload,
//            upstream_result, edgegroup, incoming_set,
//            pending_cancel_groups_set
// ARGV (5): edge_id, upstream_outcome, now_ms, flow_id, downstream_eid
//
// KEYS[10]/[11] added in Batch C item 3 for server-side
// data_passing_ref resolution. KEYS[12] added in RFC-016 Stage A for
// the per-downstream edge-group hash; KEYS[13]/[14] + ARGV[4]/[5]
// added in RFC-016 Stage C so the AnyOf/Quorum+CancelRemaining path
// can enumerate siblings and index the group in the per-partition
// `pending_cancel_groups` SET. Flow/exec partitions co-locate under
// `{fp:N}` post-RFC-011 so the FCALL remains single-slot.

ff_function! {
    pub ff_resolve_dependency(args: ResolveDependencyArgs) -> ResolveDependencyResult {
        keys(k: &ResolveDependencyKeys<'_>) {
            k.ctx.core(),
            k.ctx.deps_meta(),
            k.ctx.deps_unresolved(),
            k.ctx.dep_edge(&args.edge_id),
            k.idx.lane_eligible(k.lane_id),
            k.idx.lane_terminal(k.lane_id),
            k.idx.lane_blocked_dependencies(k.lane_id),
            k.ctx.attempt_hash(k.current_attempt_index),
            k.ctx.stream_meta(k.current_attempt_index),
            k.ctx.payload(),
            k.upstream_ctx.result(),
            k.flow_ctx.edgegroup(k.downstream_eid),
            k.flow_ctx.incoming(k.downstream_eid),
            k.flow_idx.pending_cancel_groups(),
        }
        argv {
            args.edge_id.to_string(),
            args.upstream_outcome.clone(),
            args.now.to_string(),
            k.flow_ctx.flow_id().to_owned(),
            k.downstream_eid.to_string(),
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
            other => Err(ScriptError::Parse {
                fcall: "ff_resolve_dependency".into(),
                execution_id: None,
                message: format!("unknown resolve status: {other}"),
            }),
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
            .map_err(|e| ScriptError::Parse {
                fcall: "ff_stage_dependency_edge".into(),
                execution_id: None,
                message: format!("bad edge_id: {e}"),
            })?;
        let rev: u64 = r.field_str(1).parse()
            .map_err(|e| ScriptError::Parse {
                fcall: "ff_stage_dependency_edge".into(),
                execution_id: None,
                message: format!("bad graph_revision: {e}"),
            })?;
        Ok(StageDependencyEdgeResult::Staged {
            edge_id: eid,
            new_graph_revision: rev,
        })
    }
}

// ─── ff_set_edge_group_policy (RFC-016 Stage B) ─────────────────────────
// KEYS (2): flow_core, edgegroup
// ARGV (4): policy_variant, on_satisfied, k, now_ms
//
// Stage B widens the ARGV: `k` (quorum threshold, "0" for AllOf/AnyOf)
// joins the stored fields so the resolver can evaluate Quorum without a
// separate Lua round-trip.

pub struct SetEdgeGroupPolicyKeys<'a> {
    pub fctx: &'a FlowKeyContext,
    pub downstream_eid: &'a ff_core::types::ExecutionId,
}

ff_function! {
    pub ff_set_edge_group_policy(args: SetEdgeGroupPolicyArgs) -> SetEdgeGroupPolicyResult {
        keys(k: &SetEdgeGroupPolicyKeys<'_>) {
            k.fctx.core(),
            k.fctx.edgegroup(k.downstream_eid),
        }
        argv {
            args.policy.variant_str().to_string(),
            match &args.policy {
                ff_core::contracts::EdgeDependencyPolicy::AllOf => String::new(),
                ff_core::contracts::EdgeDependencyPolicy::AnyOf { on_satisfied }
                | ff_core::contracts::EdgeDependencyPolicy::Quorum { on_satisfied, .. } => {
                    on_satisfied.variant_str().to_string()
                }
                _ => String::new(),
            },
            match &args.policy {
                ff_core::contracts::EdgeDependencyPolicy::Quorum { k, .. } => k.to_string(),
                _ => "0".to_string(),
            },
            args.now.to_string(),
        }
    }
}

impl FromFcallResult for SetEdgeGroupPolicyResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        match r.field_str(0).as_str() {
            "set" => Ok(SetEdgeGroupPolicyResult::Set),
            "already_set" => Ok(SetEdgeGroupPolicyResult::AlreadySet),
            other => Err(ScriptError::Parse {
                fcall: "ff_set_edge_group_policy".into(),
                execution_id: None,
                message: format!("unexpected status: {other}"),
            }),
        }
    }
}

// ─── ff_set_flow_tags (issue #58.4) ─────────────────────────────────────
//
// Variadic-ARGV FCALL mirroring `ff_set_execution_tags`. Hand-rolled
// since the `ff_function!` macro assumes a fixed ARGV vector.
//
// KEYS (2): flow_core, tags_key
// ARGV (>=2, even): k1, v1, k2, v2, ...

/// Call `ff_set_flow_tags`: write caller-supplied tag fields to the
/// flow's separate tags key. Returns the number of pairs applied. Tag
/// keys must match `^[a-z][a-z0-9_]*\.`. The Lua function also
/// lazy-migrates any pre-58.4 reserved-namespace fields stashed
/// inline on `flow_core` into the new tags key.
pub async fn ff_set_flow_tags(
    conn: &ferriskey::Client,
    fctx: &FlowKeyContext,
    args: &SetFlowTagsArgs,
) -> Result<SetFlowTagsResult, ScriptError> {
    let keys = [fctx.core(), fctx.tags()];
    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();

    let mut argv: Vec<String> = Vec::with_capacity(args.tags.len() * 2);
    for (k, v) in &args.tags {
        argv.push(k.clone());
        argv.push(v.clone());
    }
    let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();

    let raw = conn
        .fcall::<ferriskey::Value>("ff_set_flow_tags", &key_refs, &argv_refs)
        .await
        .map_err(ScriptError::Valkey)?;
    SetFlowTagsResult::from_fcall_result(&raw)
}

impl FromFcallResult for SetFlowTagsResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        let count: u32 = r
            .field_str(0)
            .parse()
            .map_err(|e| ScriptError::Parse {
                fcall: "ff_set_flow_tags".into(),
                execution_id: None,
                message: format!("bad tag count: {e}"),
            })?;
        Ok(SetFlowTagsResult::Ok { count })
    }
}
