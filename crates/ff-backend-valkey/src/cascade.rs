//! Post-completion cascade for the Valkey backend (PR-7b Cluster 4).
//!
//! Originally lived in `ff-engine::partition_router::dispatch_dependency_resolution`
//! as a free function that took a `ferriskey::Client` + `PartitionRouter`.
//! PR-7b moves it behind `EngineBackend::cascade_completion` so the
//! `ff-engine::completion_listener` dispatch loop can trait-route
//! through `Arc<dyn EngineBackend>` alongside the other PR-7b clusters.
//!
//! Behaviour preserved exactly — same KEYS[14]+ARGV[5], same
//! `child_skipped` 4-element response detection, same recursive cascade
//! up to [`MAX_CASCADE_DEPTH`]. Only the call site changed.
//!
//! # 6-step extraction
//!
//! The original single-function body has been split into per-stage
//! helpers so the orchestrator is small enough to read:
//!
//! 1. `read_terminal_outcome` — HGET `terminal_outcome` from `exec_core`.
//! 2. `read_outgoing_edges` — SMEMBERS the flow's `out:<eid>` set.
//! 3. `resolve_edge_target` — per-edge HGETs (downstream_eid, lane,
//!    attempt_index); fail-closed on errors.
//! 4. `invoke_resolve_dependency` — build KEYS+ARGV, FCALL, detect
//!    `child_skipped` slot-3 marker.
//! 5. `cascade_skipped_children` — recursive dispatch for descendants.
//! 6. `run_cascade` — top-level orchestrator assembling 1–5.

use ff_core::backend::CompletionPayload;
use ff_core::contracts::CascadeOutcome;
use ff_core::engine_error::EngineError;
use ff_core::keys::{ExecKeyContext, FlowIndexKeys, FlowKeyContext, IndexKeys};
use ff_core::partition::{
    Partition, PartitionConfig, execution_partition, flow_partition,
};
use ff_core::types::{AttemptIndex, EdgeId, ExecutionId, FlowId, LaneId, TimestampMs};

/// Max cascade depth to prevent runaway recursion on degenerate graphs.
const MAX_CASCADE_DEPTH: u32 = 50;

/// Entry point. Valkey `EngineBackend::cascade_completion` calls this.
pub(crate) async fn run_cascade(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    payload: &CompletionPayload,
) -> Result<CascadeOutcome, EngineError> {
    let flow_id_str = match payload.flow_id.as_ref() {
        Some(f) => f.to_string(),
        None => {
            // Not a flow member — nothing to cascade.
            return Ok(CascadeOutcome::synchronous(0, 0));
        }
    };

    let mut counters = CascadeCounters::default();
    run_inner(
        client,
        partition_config,
        &payload.execution_id,
        &flow_id_str,
        0,
        &mut counters,
    )
    .await;

    Ok(CascadeOutcome::synchronous(
        counters.resolved,
        counters.cascaded_children,
    ))
}

#[derive(Default)]
struct CascadeCounters {
    resolved: usize,
    cascaded_children: usize,
}

async fn run_inner(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    eid: &ExecutionId,
    flow_id_str: &str,
    cascade_depth: u32,
    counters: &mut CascadeCounters,
) {
    if cascade_depth > MAX_CASCADE_DEPTH {
        tracing::warn!(
            execution_id = %eid,
            cascade_depth,
            "cascade: depth limit reached, reconciler will catch remaining"
        );
        return;
    }
    if flow_id_str.is_empty() {
        return;
    }

    // Step 1 — terminal_outcome
    let exec_partition = execution_partition(eid, partition_config);
    let exec_ctx = ExecKeyContext::new(&exec_partition, eid);
    let upstream_outcome = match read_terminal_outcome(client, &exec_ctx).await {
        Some(v) => v,
        None => return,
    };

    // Step 2 — outgoing edges
    let fid = match FlowId::parse(flow_id_str) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(
                flow_id = flow_id_str,
                error = %e,
                "cascade: invalid flow_id"
            );
            return;
        }
    };
    let flow_part = flow_partition(&fid, partition_config);
    let flow_ctx = FlowKeyContext::new(&flow_part, &fid);
    let flow_idx = FlowIndexKeys::new(&flow_part);
    let edge_ids = match read_outgoing_edges(client, &flow_ctx, eid).await {
        Some(ids) if !ids.is_empty() => ids,
        _ => return,
    };

    let now_ms = TimestampMs::now().0.to_string();
    let mut resolved_this_level: u32 = 0;
    let mut skipped_children: Vec<(ExecutionId, String)> = Vec::new();

    for edge_id in &edge_ids {
        // Parse edge id up front — a non-parsing adjacency entry is
        // corruption; skip + warn rather than silently default.
        let parsed_edge_id = match EdgeId::parse(edge_id) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(
                    edge_id = edge_id.as_str(),
                    flow_id = flow_id_str,
                    error = %e,
                    "cascade: invalid edge_id in outgoing adjacency set, skipping"
                );
                continue;
            }
        };

        // Step 3 — per-edge target
        let Some(target) = resolve_edge_target(
            client,
            partition_config,
            &flow_ctx,
            &parsed_edge_id,
            edge_id,
        )
        .await
        else {
            continue;
        };

        // Step 4 — FCALL + child_skipped detection
        let outcome = invoke_resolve_dependency(
            client,
            &flow_ctx,
            &flow_idx,
            &target,
            &parsed_edge_id,
            edge_id,
            eid,
            upstream_outcome.as_str(),
            &now_ms,
            flow_id_str,
        )
        .await;

        if let EdgeInvokeOutcome::Resolved { child_skipped } = outcome {
            counters.resolved += 1;
            resolved_this_level += 1;
            if child_skipped {
                skipped_children
                    .push((target.downstream_eid.clone(), flow_id_str.to_string()));
            }
        }
    }

    if resolved_this_level > 0 {
        tracing::info!(
            execution_id = %eid,
            flow_id = flow_id_str,
            resolved = resolved_this_level,
            total_edges = edge_ids.len(),
            skipped_cascade = skipped_children.len(),
            "cascade: dependency resolution complete"
        );
    }

    // Step 5 — cascade skipped children
    cascade_skipped_children(
        client,
        partition_config,
        &skipped_children,
        cascade_depth,
        counters,
    )
    .await;
}

// ── Step 1 ──────────────────────────────────────────────────────────

async fn read_terminal_outcome(
    client: &ferriskey::Client,
    exec_ctx: &ExecKeyContext,
) -> Option<String> {
    let core_key = exec_ctx.core();
    match client
        .cmd("HGET")
        .arg(&core_key)
        .arg("terminal_outcome")
        .execute::<Option<String>>()
        .await
    {
        Ok(v) => Some(v.unwrap_or_default()),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "cascade: failed to read terminal_outcome"
            );
            None
        }
    }
}

// ── Step 2 ──────────────────────────────────────────────────────────

async fn read_outgoing_edges(
    client: &ferriskey::Client,
    flow_ctx: &FlowKeyContext,
    eid: &ExecutionId,
) -> Option<Vec<String>> {
    let out_key = flow_ctx.outgoing(eid);
    match client
        .cmd("SMEMBERS")
        .arg(&out_key)
        .execute::<Vec<String>>()
        .await
    {
        Ok(ids) => Some(ids),
        Err(e) => {
            tracing::warn!(
                execution_id = %eid,
                error = %e,
                "cascade: SMEMBERS outgoing failed"
            );
            None
        }
    }
}

// ── Step 3 ──────────────────────────────────────────────────────────

struct EdgeTarget {
    downstream_eid: ExecutionId,
    child_partition: Partition,
    lane_id: LaneId,
    attempt_index: AttemptIndex,
}

async fn resolve_edge_target(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    flow_ctx: &FlowKeyContext,
    parsed_edge_id: &EdgeId,
    edge_id_str: &str,
) -> Option<EdgeTarget> {
    // Read downstream_execution_id from the edge record.
    let edge_key = flow_ctx.edge(parsed_edge_id);
    let downstream_eid_str: Option<String> = match client
        .cmd("HGET")
        .arg(&edge_key)
        .arg("downstream_execution_id")
        .execute::<Option<String>>()
        .await
    {
        Ok(v) => v,
        Err(_) => return None,
    };
    let downstream_eid_str = match downstream_eid_str {
        Some(s) if !s.is_empty() => s,
        _ => return None,
    };
    let downstream_eid = ExecutionId::parse(&downstream_eid_str).ok()?;

    let child_partition = execution_partition(&downstream_eid, partition_config);
    let child_ctx = ExecKeyContext::new(&child_partition, &downstream_eid);
    let child_core_key = child_ctx.core();

    // Fail-closed: transport error must NOT default to "default" lane.
    let lane_str: Option<String> = match client
        .cmd("HGET")
        .arg(&child_core_key)
        .arg("lane_id")
        .execute::<Option<String>>()
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                edge_id = edge_id_str,
                downstream = downstream_eid_str.as_str(),
                error = %e,
                "cascade: HGET lane_id failed, skipping (reconciler will retry)"
            );
            return None;
        }
    };
    let lane_id = LaneId::new(lane_str.as_deref().unwrap_or("default"));

    // current_attempt_index: absence (None) legitimately means attempt 0;
    // only a transport-read failure skips.
    let att_idx_str: Option<String> = match client
        .cmd("HGET")
        .arg(&child_core_key)
        .arg("current_attempt_index")
        .execute::<Option<String>>()
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                edge_id = edge_id_str,
                downstream = downstream_eid_str.as_str(),
                error = %e,
                "cascade: HGET current_attempt_index failed, \
                 skipping (reconciler will retry)"
            );
            return None;
        }
    };
    let attempt_index = AttemptIndex::new(
        att_idx_str
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
    );

    Some(EdgeTarget {
        downstream_eid,
        child_partition,
        lane_id,
        attempt_index,
    })
}

// ── Step 4 ──────────────────────────────────────────────────────────

enum EdgeInvokeOutcome {
    Resolved { child_skipped: bool },
    FcallFailed,
}

#[allow(clippy::too_many_arguments)]
async fn invoke_resolve_dependency(
    client: &ferriskey::Client,
    flow_ctx: &FlowKeyContext,
    flow_idx: &FlowIndexKeys,
    target: &EdgeTarget,
    parsed_edge_id: &EdgeId,
    edge_id_str: &str,
    upstream_eid: &ExecutionId,
    upstream_outcome: &str,
    now_ms: &str,
    flow_id_str: &str,
) -> EdgeInvokeOutcome {
    let child_ctx = ExecKeyContext::new(&target.child_partition, &target.downstream_eid);
    let child_idx = IndexKeys::new(&target.child_partition);
    let child_core_key = child_ctx.core();
    let deps_meta = child_ctx.deps_meta();
    let unresolved = child_ctx.deps_unresolved();
    let eligible = child_idx.lane_eligible(&target.lane_id);
    let terminal = child_idx.lane_terminal(&target.lane_id);
    let blocked_deps = child_idx.lane_blocked_dependencies(&target.lane_id);
    let attempt_hash = child_ctx.attempt_hash(target.attempt_index);
    let stream_meta = child_ctx.stream_meta(target.attempt_index);
    let downstream_payload = child_ctx.payload();
    // Upstream + downstream co-locate via flow membership (RFC-011 §7.3),
    // so build the upstream key on the child's partition.
    let upstream_ctx = ExecKeyContext::new(&target.child_partition, upstream_eid);
    let upstream_result = upstream_ctx.result();
    let dep_hash = child_ctx.dep_edge(parsed_edge_id);

    let edgegroup = flow_ctx.edgegroup(&target.downstream_eid);
    let incoming_set = flow_ctx.incoming(&target.downstream_eid);
    let pending_cancel_groups = flow_idx.pending_cancel_groups();
    let downstream_eid_full = target.downstream_eid.to_string();

    // KEYS (14): exec_core, deps_meta, unresolved_set, dep_hash,
    //            eligible_zset, terminal_zset, blocked_deps_zset,
    //            attempt_hash, stream_meta, downstream_payload,
    //            upstream_result, edgegroup, incoming_set,
    //            pending_cancel_groups
    let keys: [&str; 14] = [
        &child_core_key,
        &deps_meta,
        &unresolved,
        &dep_hash,
        &eligible,
        &terminal,
        &blocked_deps,
        &attempt_hash,
        &stream_meta,
        &downstream_payload,
        &upstream_result,
        &edgegroup,
        &incoming_set,
        &pending_cancel_groups,
    ];
    // ARGV (5): edge_id, upstream_outcome, now_ms, flow_id, downstream_eid
    let argv: [&str; 5] = [
        edge_id_str,
        upstream_outcome,
        now_ms,
        flow_id_str,
        &downstream_eid_full,
    ];

    match client
        .fcall::<ferriskey::Value>("ff_resolve_dependency", &keys, &argv)
        .await
    {
        Ok(val) => {
            tracing::debug!(
                edge_id = edge_id_str,
                downstream = downstream_eid_full.as_str(),
                outcome = upstream_outcome,
                "cascade: resolved dependency"
            );
            EdgeInvokeOutcome::Resolved {
                child_skipped: is_child_skipped_result(&val),
            }
        }
        Err(e) => {
            tracing::warn!(
                edge_id = edge_id_str,
                downstream = downstream_eid_full.as_str(),
                error = %e,
                "cascade: ff_resolve_dependency failed"
            );
            EdgeInvokeOutcome::FcallFailed
        }
    }
}

// ── Step 5 ──────────────────────────────────────────────────────────

async fn cascade_skipped_children(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    skipped: &[(ExecutionId, String)],
    cascade_depth: u32,
    counters: &mut CascadeCounters,
) {
    for (child_eid, child_flow_id) in skipped {
        counters.cascaded_children += 1;
        Box::pin(run_inner(
            client,
            partition_config,
            child_eid,
            child_flow_id,
            cascade_depth + 1,
            counters,
        ))
        .await;
    }
}

// ── child_skipped detector (moved verbatim from partition_router) ───

/// Check if an `ff_resolve_dependency` result indicates the child was
/// skipped. Result shapes after Batch C item 3:
///   `[1, "OK", "already_resolved"]`            — 3 elements total
///   `[1, "OK", "satisfied", ""|"data_injected"]` — 4 elements total
///   `[1, "OK", "impossible", ""|"child_skipped"]` — 4 elements total
///
/// "Child skipped" lives in slot 3 (0-indexed) on the impossible path
/// only; the satisfied path's fourth slot is the unrelated
/// `data_injected` marker.
fn is_child_skipped_result(value: &ferriskey::Value) -> bool {
    match value {
        ferriskey::Value::Array(arr) => {
            if arr.len() < 4 {
                // 3-element responses are normal (already_resolved).
                return false;
            }
            arr.get(3)
                .and_then(|v| match v {
                    Ok(ferriskey::Value::BulkString(b)) => Some(&b[..] == b"child_skipped"),
                    Ok(ferriskey::Value::SimpleString(s)) => Some(s == "child_skipped"),
                    _ => None,
                })
                .unwrap_or(false)
        }
        _ => {
            tracing::warn!(
                "is_child_skipped_result: expected Array, got non-array value"
            );
            false
        }
    }
}
