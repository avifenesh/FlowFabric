//! Dependency resolution reconciler.
//!
//! Safety net for cross-partition dependency resolution. When an upstream
//! execution completes, ff-engine::partition_router normally calls ff_resolve_dependency
//! on each downstream child. If the engine crashes between the upstream's
//! completion and the child resolution dispatch, children remain stuck in
//! blocked_by_dependencies. This reconciler detects and resolves that gap.
//!
//! For each blocked execution: reads deps:unresolved SET, cross-partition
//! reads upstream exec_core to check if terminal, calls ff_resolve_dependency
//! if upstream is terminal.
//!
//! Reference: RFC-007 §Resolve dependency, RFC-010 §6.14

use std::collections::HashMap;
use std::time::Duration;

use ff_core::keys::IndexKeys;
use ff_core::partition::{Partition, PartitionConfig, PartitionFamily, execution_partition};
use ff_core::types::{ExecutionId, LaneId};

use super::{ScanResult, Scanner};

const BATCH_SIZE: u32 = 50;
/// Max dep edges to resolve per execution per cycle.
const MAX_EDGES_PER_EXEC: usize = 20;

pub struct DependencyReconciler {
    interval: Duration,
    lanes: Vec<LaneId>,
    partition_config: PartitionConfig,
}

impl DependencyReconciler {
    pub fn new(interval: Duration, lanes: Vec<LaneId>, partition_config: PartitionConfig) -> Self {
        Self { interval, lanes, partition_config }
    }
}

impl Scanner for DependencyReconciler {
    fn name(&self) -> &'static str {
        "dependency_reconciler"
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    async fn scan_partition(
        &self,
        client: &ferriskey::Client,
        partition: u16,
    ) -> ScanResult {
        let p = Partition {
            family: PartitionFamily::Execution,
            index: partition,
        };
        let idx = IndexKeys::new(&p);
        let tag = p.hash_tag();

        let mut total_processed: u32 = 0;
        let mut total_errors: u32 = 0;

        // Cross-partition cache: upstream_eid → terminal_outcome (or empty if not terminal)
        let mut upstream_cache: HashMap<String, String> = HashMap::new();

        for lane in &self.lanes {
            let blocked_key = idx.lane_blocked_dependencies(lane);

            let blocked: Vec<String> = match client
                .cmd("ZRANGEBYSCORE")
                .arg(&blocked_key)
                .arg("-inf")
                .arg("+inf")
                .arg("LIMIT")
                .arg("0")
                .arg(BATCH_SIZE.to_string().as_str())
                .execute()
                .await
            {
                Ok(ids) => ids,
                Err(e) => {
                    tracing::warn!(
                        partition, error = %e,
                        "dependency_reconciler: ZRANGEBYSCORE blocked:deps failed"
                    );
                    total_errors += 1;
                    continue;
                }
            };

            for eid_str in &blocked {
                match reconcile_one_execution(
                    client, &tag, &idx, lane, eid_str,
                    &mut upstream_cache, &self.partition_config,
                ).await {
                    Ok(n) => total_processed += n,
                    Err(e) => {
                        tracing::warn!(
                            partition,
                            execution_id = eid_str.as_str(),
                            error = %e,
                            "dependency_reconciler: reconcile failed"
                        );
                        total_errors += 1;
                    }
                }
            }
        }

        ScanResult { processed: total_processed, errors: total_errors }
    }
}

/// Reconcile one blocked execution. Returns count of edges resolved.
async fn reconcile_one_execution(
    client: &ferriskey::Client,
    tag: &str,
    idx: &IndexKeys,
    lane: &LaneId,
    eid_str: &str,
    upstream_cache: &mut HashMap<String, String>,
    config: &PartitionConfig,
) -> Result<u32, ferriskey::Error> {
    let deps_unresolved_key = format!("ff:exec:{}:{}:deps:unresolved", tag, eid_str);

    // Read unresolved dep edge IDs
    let edge_ids: Vec<String> = client
        .cmd("SMEMBERS")
        .arg(&deps_unresolved_key)
        .execute()
        .await
        .unwrap_or_default();

    if edge_ids.is_empty() {
        return Ok(0);
    }

    let mut resolved: u32 = 0;

    for (i, edge_id) in edge_ids.iter().enumerate() {
        if i >= MAX_EDGES_PER_EXEC {
            break; // limit per cycle
        }

        // Read the dep edge to find upstream_execution_id
        let dep_key = format!("ff:exec:{}:{}:dep:{}", tag, eid_str, edge_id);
        let upstream_id: Option<String> = client
            .cmd("HGET")
            .arg(&dep_key)
            .arg("upstream_execution_id")
            .execute()
            .await?;

        let upstream_id = match upstream_id {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };

        // Check upstream terminal state (cross-partition, cached)
        let terminal_outcome = get_upstream_outcome(
            client, &upstream_id, upstream_cache, config,
        ).await;

        if terminal_outcome.is_empty() {
            continue; // upstream not terminal yet
        }

        // Upstream is terminal — resolve the dependency.
        // Pass the actual terminal_outcome as upstream_outcome ARGV.
        // The Lua checks == "success" for the satisfaction path; anything
        // else triggers the impossible path. Using the real outcome
        // maintains semantic correctness for audit and future extensions.
        let resolution = terminal_outcome.as_str();

        let now_ms = match crate::scanner::lease_expiry::server_time_ms(client).await {
            Ok(t) => t,
            Err(_) => continue,
        };

        // Build KEYS for ff_resolve_dependency
        let exec_core = format!("ff:exec:{}:{}:core", tag, eid_str);
        let deps_meta = format!("ff:exec:{}:{}:deps:meta", tag, eid_str);
        let eligible_key = idx.lane_eligible(lane);
        let blocked_deps_key = idx.lane_blocked_dependencies(lane);
        let terminal_key = idx.lane_terminal(lane);

        // For attempt_hash and stream_meta, read current_attempt_index
        let att_idx_str: Option<String> = client
            .cmd("HGET")
            .arg(&exec_core)
            .arg("current_attempt_index")
            .execute()
            .await?;
        let att_idx = att_idx_str.as_deref().unwrap_or("0");
        let attempt_hash = format!("ff:attempt:{}:{}:{}", tag, eid_str, att_idx);
        let stream_meta = format!("ff:stream:{}:{}:{}:meta", tag, eid_str, att_idx);

        // KEYS must match Lua positional order:
        // [1] exec_core, [2] deps_meta, [3] unresolved_set, [4] dep_hash,
        // [5] eligible_zset, [6] terminal_zset, [7] blocked_deps_zset,
        // [8] attempt_hash, [9] stream_meta
        let keys: [&str; 9] = [
            &exec_core,           // 1
            &deps_meta,           // 2
            &deps_unresolved_key, // 3
            &dep_key,             // 4
            &eligible_key,        // 5
            &terminal_key,        // 6
            &blocked_deps_key,    // 7
            &attempt_hash,        // 8
            &stream_meta,         // 9
        ];
        let now_s = now_ms.to_string();
        let argv: [&str; 3] = [edge_id.as_str(), resolution, &now_s];

        match client
            .fcall::<ferriskey::Value>("ff_resolve_dependency", &keys, &argv)
            .await
        {
            Ok(_) => {
                resolved += 1;
                tracing::debug!(
                    execution_id = eid_str,
                    edge_id = edge_id.as_str(),
                    upstream_id = upstream_id.as_str(),
                    resolution,
                    "dependency_reconciler: resolved stale dependency"
                );
            }
            Err(e) => {
                tracing::warn!(
                    execution_id = eid_str,
                    edge_id = edge_id.as_str(),
                    error = %e,
                    "dependency_reconciler: ff_resolve_dependency failed"
                );
            }
        }
    }

    Ok(resolved)
}

/// Get the terminal outcome of an upstream execution (cross-partition, cached).
/// Returns empty string if not terminal.
async fn get_upstream_outcome(
    client: &ferriskey::Client,
    upstream_id: &str,
    cache: &mut HashMap<String, String>,
    config: &PartitionConfig,
) -> String {
    if let Some(outcome) = cache.get(upstream_id) {
        return outcome.clone();
    }

    // Compute upstream's partition
    let eid = match ExecutionId::parse(upstream_id) {
        Ok(id) => id,
        Err(_) => {
            cache.insert(upstream_id.to_owned(), String::new());
            return String::new();
        }
    };
    let partition = execution_partition(&eid, config);
    let upstream_tag = partition.hash_tag();
    let upstream_core = format!("ff:exec:{}:{}:core", upstream_tag, upstream_id);

    // Read lifecycle_phase + terminal_outcome
    let fields: Vec<Option<String>> = client
        .cmd("HMGET")
        .arg(&upstream_core)
        .arg("lifecycle_phase")
        .arg("terminal_outcome")
        .execute()
        .await
        .unwrap_or_default();

    let lifecycle = fields.first().and_then(|v| v.clone()).unwrap_or_default();
    let outcome = fields.get(1).and_then(|v| v.clone()).unwrap_or_default();

    let result = if lifecycle == "terminal" && !outcome.is_empty() && outcome != "none" {
        outcome
    } else {
        String::new()
    };

    cache.insert(upstream_id.to_owned(), result.clone());
    result
}
