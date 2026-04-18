//! Partition-aware dispatch for cross-partition operations.
//!
//! The PartitionRouter resolves an ExecutionId to its partition and provides
//! key contexts for Valkey operations. For Phase 1, a single ferriskey::Client
//! connection is shared across all partitions (the client handles cluster routing
//! internally via hash tags).

use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{
    Partition, PartitionConfig, PartitionFamily, execution_partition,
};
use ff_core::types::ExecutionId;

/// Routes execution operations to the correct partition.
///
/// In a Valkey Cluster deployment, the ferriskey client handles slot-level
/// routing transparently — all keys for a partition share the same `{p:N}`
/// hash tag, so they land on the same shard. The router's job is partition
/// computation and key context construction, not connection selection.
pub struct PartitionRouter {
    config: PartitionConfig,
}

impl PartitionRouter {
    pub fn new(config: PartitionConfig) -> Self {
        Self { config }
    }

    /// Resolve an execution ID to its partition.
    pub fn partition_for(&self, eid: &ExecutionId) -> Partition {
        execution_partition(eid, &self.config)
    }

    /// Build an ExecKeyContext for the given execution.
    pub fn exec_keys(&self, eid: &ExecutionId) -> ExecKeyContext {
        let partition = self.partition_for(eid);
        ExecKeyContext::new(&partition, eid)
    }

    /// Build IndexKeys for a given partition index.
    pub fn index_keys(&self, partition_index: u16) -> IndexKeys {
        let partition = Partition {
            family: PartitionFamily::Execution,
            index: partition_index,
        };
        IndexKeys::new(&partition)
    }

    /// The partition config.
    pub fn config(&self) -> &PartitionConfig {
        &self.config
    }

    /// Total number of flow partitions.
    ///
    /// Post-RFC-011: exec keys co-locate with their parent flow's partition
    /// under hash-tag routing, so this count governs exec routing too.
    /// There is no separate `num_execution_partitions`.
    pub fn num_flow_partitions(&self) -> u16 {
        self.config.num_flow_partitions
    }
}

/// Post-completion dependency dispatch.
///
/// After an execution completes/fails/cancels, the engine dispatches
/// `resolve_dependency` to each downstream child's `{p:N}` partition.
/// Reads outgoing edges from the flow partition, then for each downstream
/// child, calls FCALL ff_resolve_dependency on the child's partition.
///
/// If a child is transitioned to terminal (skipped) by the resolution,
/// cascades dispatch for the skipped child's outgoing edges so that
/// grandchildren don't wait for the reconciler (up to 15s/level).
pub async fn dispatch_dependency_resolution(
    client: &ferriskey::Client,
    router: &PartitionRouter,
    eid: &ExecutionId,
    flow_id: Option<&str>,
) {
    dispatch_dependency_resolution_inner(client, router, eid, flow_id, 0).await;
}

/// Max cascade depth to prevent runaway recursion on degenerate graphs.
const MAX_CASCADE_DEPTH: u32 = 50;

async fn dispatch_dependency_resolution_inner(
    client: &ferriskey::Client,
    router: &PartitionRouter,
    eid: &ExecutionId,
    flow_id: Option<&str>,
    cascade_depth: u32,
) {
    if cascade_depth > MAX_CASCADE_DEPTH {
        tracing::warn!(
            execution_id = %eid,
            cascade_depth,
            "dispatch_dep: cascade depth limit reached, reconciler will catch remaining"
        );
        return;
    }

    let flow_id_str = match flow_id {
        Some(fid) if !fid.is_empty() => fid,
        _ => return, // not in a flow
    };

    // Read terminal_outcome from exec_core to determine resolution type
    let exec_ctx = router.exec_keys(eid);
    let core_key = exec_ctx.core();
    let outcome: Option<String> = match client
        .cmd("HGET")
        .arg(&core_key)
        .arg("terminal_outcome")
        .execute()
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                execution_id = %eid,
                error = %e,
                "dispatch_dep: failed to read terminal_outcome"
            );
            return;
        }
    };

    let outcome_str = outcome.unwrap_or_default();
    // Pass the actual terminal_outcome as upstream_outcome ARGV.
    // The Lua checks == "success" for the satisfaction path; anything
    // else (failed, cancelled, expired, skipped) triggers the impossible path.
    let upstream_outcome = outcome_str.as_str();

    // Read outgoing edges from flow partition.
    // First, compute flow partition.
    let fid = match ff_core::types::FlowId::parse(flow_id_str) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(
                flow_id = flow_id_str,
                error = %e,
                "dispatch_dep: invalid flow_id"
            );
            return;
        }
    };

    let flow_partition = ff_core::partition::flow_partition(&fid, router.config());
    let flow_ctx = ff_core::keys::FlowKeyContext::new(&flow_partition, &fid);

    // Read outgoing adjacency set: ff:flow:{fp:N}:<flow_id>:out:<execution_id>
    let out_key = flow_ctx.outgoing(eid);
    let edge_ids: Vec<String> = match client
        .cmd("SMEMBERS")
        .arg(&out_key)
        .execute()
        .await
    {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(
                execution_id = %eid,
                flow_id = flow_id_str,
                error = %e,
                "dispatch_dep: SMEMBERS outgoing failed"
            );
            return;
        }
    };

    if edge_ids.is_empty() {
        return;
    }

    let now_ms = ff_core::types::TimestampMs::now().0.to_string();
    let mut resolved: u32 = 0;
    let mut skipped_children: Vec<(ExecutionId, String)> = Vec::new();

    for edge_id in &edge_ids {
        // Read the edge record from flow partition to find downstream_execution_id
        let edge_key = flow_ctx.edge(&ff_core::types::EdgeId::parse(edge_id).unwrap_or_default());
        let downstream_eid_str: Option<String> = match client
            .cmd("HGET")
            .arg(&edge_key)
            .arg("downstream_execution_id")
            .execute()
            .await
        {
            Ok(v) => v,
            Err(_) => continue,
        };

        let downstream_eid_str = match downstream_eid_str {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };

        let downstream_eid = match ExecutionId::parse(&downstream_eid_str) {
            Ok(id) => id,
            Err(_) => continue,
        };

        // Compute child's partition and build keys for ff_resolve_dependency
        let child_partition = router.partition_for(&downstream_eid);
        let child_ctx = ExecKeyContext::new(&child_partition, &downstream_eid);
        let child_idx = IndexKeys::new(&child_partition);

        // Read child's lane_id for lane-scoped index keys
        let child_core_key = child_ctx.core();
        let lane_str: Option<String> = client
            .cmd("HGET")
            .arg(&child_core_key)
            .arg("lane_id")
            .execute()
            .await
            .unwrap_or(None);
        let lane_id = ff_core::types::LaneId::new(
            lane_str.as_deref().unwrap_or("default"),
        );

        let att_idx_str: Option<String> = client
            .cmd("HGET")
            .arg(&child_core_key)
            .arg("current_attempt_index")
            .execute()
            .await
            .unwrap_or(None);
        let att_idx = ff_core::types::AttemptIndex::new(
            att_idx_str.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0),
        );

        let dep_hash = child_ctx.dep_edge(
            &ff_core::types::EdgeId::parse(edge_id).unwrap_or_default(),
        );

        // KEYS (9): exec_core, deps_meta, unresolved_set, dep_hash,
        //           eligible_zset, terminal_zset, blocked_deps_zset,
        //           attempt_hash, stream_meta
        let deps_meta = child_ctx.deps_meta();
        let unresolved = child_ctx.deps_unresolved();
        let eligible = child_idx.lane_eligible(&lane_id);
        let terminal = child_idx.lane_terminal(&lane_id);
        let blocked_deps = child_idx.lane_blocked_dependencies(&lane_id);
        let attempt_hash = child_ctx.attempt_hash(att_idx);
        let stream_meta = child_ctx.stream_meta(att_idx);

        let keys: [&str; 9] = [
            &child_core_key,  // 1
            &deps_meta,       // 2
            &unresolved,      // 3
            &dep_hash,        // 4
            &eligible,        // 5
            &terminal,        // 6
            &blocked_deps,    // 7
            &attempt_hash,    // 8
            &stream_meta,     // 9
        ];
        let argv: [&str; 3] = [edge_id, upstream_outcome, &now_ms];

        match client
            .fcall::<ferriskey::Value>("ff_resolve_dependency", &keys, &argv)
            .await
        {
            Ok(val) => {
                resolved += 1;
                tracing::debug!(
                    edge_id = edge_id.as_str(),
                    downstream = downstream_eid_str.as_str(),
                    outcome = upstream_outcome,
                    "dispatch_dep: resolved dependency"
                );
                // Check if child was skipped (transitioned to terminal).
                // If so, cascade dispatch for the child's outgoing edges.
                if is_child_skipped_result(&val) {
                    skipped_children.push((
                        downstream_eid.clone(),
                        flow_id_str.to_string(),
                    ));
                }
            }
            Err(e) => {
                tracing::warn!(
                    edge_id = edge_id.as_str(),
                    downstream = downstream_eid_str.as_str(),
                    error = %e,
                    "dispatch_dep: ff_resolve_dependency failed"
                );
            }
        }
    }

    if resolved > 0 {
        tracing::info!(
            execution_id = %eid,
            flow_id = flow_id_str,
            resolved,
            total_edges = edge_ids.len(),
            skipped_cascade = skipped_children.len(),
            "dispatch_dep: dependency resolution complete"
        );
    }

    // Cascade: dispatch for children that were skipped by dependency
    // impossibility. Without this, grandchildren stay blocked until the
    // dependency_reconciler picks them up (up to reconciler_interval per level).
    for (child_eid, child_flow_id) in &skipped_children {
        Box::pin(dispatch_dependency_resolution_inner(
            client, router, child_eid, Some(child_flow_id.as_str()),
            cascade_depth + 1,
        )).await;
    }
}

/// Check if an ff_resolve_dependency result indicates the child was skipped.
/// Result format: [1, "OK", "impossible", "child_skipped"]
fn is_child_skipped_result(value: &ferriskey::Value) -> bool {
    match value {
        ferriskey::Value::Array(arr) => {
            if arr.len() < 4 {
                if arr.len() != 2 {
                    tracing::warn!(
                        arr_len = arr.len(),
                        "is_child_skipped_result: unexpected array length (expected 2 or 4)"
                    );
                }
                return false;
            }
            arr.get(3)
                .and_then(|v| match v {
                    Ok(ferriskey::Value::BulkString(b)) => {
                        Some(&b[..] == b"child_skipped")
                    }
                    Ok(ferriskey::Value::SimpleString(s)) => {
                        Some(s == "child_skipped")
                    }
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
