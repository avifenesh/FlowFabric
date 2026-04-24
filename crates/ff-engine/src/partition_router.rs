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
        // Parse the edge id once up front — an adjacency-set entry that
        // doesn't round-trip through EdgeId::parse is either corruption
        // or a legacy marker. Defaulting to a fresh random UUID (the
        // pre-fix behaviour of `unwrap_or_default()` via the uuid_id!
        // macro) would point at a non-existent edge and mask the issue.
        let parsed_edge_id = match ff_core::types::EdgeId::parse(edge_id) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(
                    edge_id = edge_id.as_str(),
                    flow_id = flow_id_str,
                    error = %e,
                    "dispatch_dep: invalid edge_id in outgoing adjacency set, skipping"
                );
                continue;
            }
        };

        // Read the edge record from flow partition to find downstream_execution_id
        let edge_key = flow_ctx.edge(&parsed_edge_id);
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

        // Read child's lane_id for lane-scoped index keys. Fail-closed:
        // a transport error must NOT silently default to "default",
        // which would mutate the wrong lane's eligible/terminal/blocked
        // ZSETs on the ff_resolve_dependency FCALL below. Skip this
        // edge; the dependency_reconciler will retry on its next pass.
        let child_core_key = child_ctx.core();
        let lane_str: Option<String> = match client
            .cmd("HGET")
            .arg(&child_core_key)
            .arg("lane_id")
            .execute()
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    edge_id = edge_id.as_str(),
                    downstream = downstream_eid_str.as_str(),
                    error = %e,
                    "dispatch_dep: HGET lane_id failed, skipping (reconciler will retry)"
                );
                continue;
            }
        };
        let lane_id = ff_core::types::LaneId::new(
            lane_str.as_deref().unwrap_or("default"),
        );

        // current_attempt_index: same treatment as lane_id. Absence
        // (None) legitimately means the child is at attempt 0; only a
        // read failure skips.
        let att_idx_str: Option<String> = match client
            .cmd("HGET")
            .arg(&child_core_key)
            .arg("current_attempt_index")
            .execute()
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    edge_id = edge_id.as_str(),
                    downstream = downstream_eid_str.as_str(),
                    error = %e,
                    "dispatch_dep: HGET current_attempt_index failed, \
                     skipping (reconciler will retry)"
                );
                continue;
            }
        };
        let att_idx = ff_core::types::AttemptIndex::new(
            att_idx_str.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0),
        );

        let dep_hash = child_ctx.dep_edge(&parsed_edge_id);

        // KEYS (11): exec_core, deps_meta, unresolved_set, dep_hash,
        //            eligible_zset, terminal_zset, blocked_deps_zset,
        //            attempt_hash, stream_meta, downstream_payload,
        //            upstream_result
        // KEYS[10]/[11] added for Batch C item 3: server-side
        // data_passing_ref resolution. Upstream and downstream are
        // co-located via flow membership (RFC-011 §7.3), so we build
        // the upstream key on the child's partition using the same
        // ExecKeyContext shape.
        let deps_meta = child_ctx.deps_meta();
        let unresolved = child_ctx.deps_unresolved();
        let eligible = child_idx.lane_eligible(&lane_id);
        let terminal = child_idx.lane_terminal(&lane_id);
        let blocked_deps = child_idx.lane_blocked_dependencies(&lane_id);
        let attempt_hash = child_ctx.attempt_hash(att_idx);
        let stream_meta = child_ctx.stream_meta(att_idx);
        let downstream_payload = child_ctx.payload();
        let upstream_ctx = ExecKeyContext::new(&child_partition, eid);
        let upstream_result = upstream_ctx.result();

        let edgegroup = flow_ctx.edgegroup(&downstream_eid);
        let incoming_set = flow_ctx.incoming(&downstream_eid);
        let pending_cancel_groups = ff_core::keys::FlowIndexKeys::new(&flow_partition)
            .pending_cancel_groups();
        let downstream_eid_full = downstream_eid.to_string();
        let keys: [&str; 14] = [
            &child_core_key,         // 1
            &deps_meta,              // 2
            &unresolved,             // 3
            &dep_hash,               // 4
            &eligible,               // 5
            &terminal,               // 6
            &blocked_deps,           // 7
            &attempt_hash,           // 8
            &stream_meta,            // 9
            &downstream_payload,     // 10
            &upstream_result,        // 11
            &edgegroup,              // 12 (RFC-016 Stage A)
            &incoming_set,           // 13 (RFC-016 Stage C)
            &pending_cancel_groups,  // 14 (RFC-016 Stage C)
        ];
        let argv: [&str; 5] = [
            edge_id,
            upstream_outcome,
            &now_ms,
            flow_id_str,             // 4 (RFC-016 Stage C)
            &downstream_eid_full,    // 5 (RFC-016 Stage C)
        ];

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

/// Postgres-backend parallel to [`dispatch_dependency_resolution`].
///
/// Wave 5a (RFC-v0.7 migration-master Part 3 hotspot #1). Delegates
/// to [`ff_backend_postgres::dispatch::dispatch_completion`], which
/// implements the same cascade semantics as the Valkey
/// `ff_resolve_dependency` FCALL but under the per-hop-transaction
/// rule adjudicated in K-2 of the RFC round-2 debate: each
/// downstream `ff_edge_group` advance runs in its own serializable
/// tx so the cascade never holds a lock across transitive
/// descendants.
///
/// The engine's dispatch loop picks this branch when the deployment
/// configures the Postgres backend; the Valkey branch above stays
/// untouched. Keyed on `event_id` (the `ff_completion_event`
/// bigserial primary key), NOT on `execution_id`, so a replay of the
/// same completion event short-circuits via the
/// `dispatched_at_ms IS NULL` claim in the dispatcher.
#[cfg(feature = "postgres")]
pub async fn dispatch_via_postgres(
    pool: &ff_backend_postgres::PgPool,
    event_id: i64,
) -> Result<ff_backend_postgres::dispatch::DispatchOutcome, ff_core::engine_error::EngineError> {
    ff_backend_postgres::dispatch::dispatch_completion(pool, event_id).await
}

/// Check if an ff_resolve_dependency result indicates the child was
/// skipped. Result shapes after Batch C item 3:
///   `[1, "OK", "already_resolved"]`            — 3 elements total
///   `[1, "OK", "satisfied", ""|"data_injected"]` — 4 elements total
///   `[1, "OK", "impossible", ""|"child_skipped"]` — 4 elements total
///
/// "Child skipped" lives in slot 3 (0-indexed) on the impossible path
/// only; the satisfied path's fourth slot is the unrelated
/// data_injected marker.
fn is_child_skipped_result(value: &ferriskey::Value) -> bool {
    match value {
        ferriskey::Value::Array(arr) => {
            if arr.len() < 4 {
                // 3-element responses are normal (already_resolved).
                // No warning.
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
