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
use std::sync::Arc;
use std::time::Duration;

use ff_core::backend::ScannerFilter;
use ff_core::engine_backend::EngineBackend;
use ff_core::keys::IndexKeys;
use ff_core::partition::{Partition, PartitionConfig, PartitionFamily, execution_partition};
use ff_core::types::{ExecutionId, LaneId};

use super::{should_skip_candidate, ScanResult, Scanner};

const BATCH_SIZE: u32 = 50;
/// Max dep edges to resolve per execution per cycle.
const MAX_EDGES_PER_EXEC: usize = 20;

pub struct DependencyReconciler {
    interval: Duration,
    lanes: Vec<LaneId>,
    partition_config: PartitionConfig,
    filter: ScannerFilter,
    backend: Option<Arc<dyn EngineBackend>>,
}

impl DependencyReconciler {
    pub fn new(interval: Duration, lanes: Vec<LaneId>, partition_config: PartitionConfig) -> Self {
        Self::with_filter(interval, lanes, partition_config, ScannerFilter::default())
    }

    /// Construct with a [`ScannerFilter`] applied per candidate
    /// (issue #122).
    pub fn with_filter(
        interval: Duration,
        lanes: Vec<LaneId>,
        partition_config: PartitionConfig,
        filter: ScannerFilter,
    ) -> Self {
        Self {
            interval,
            lanes,
            partition_config,
            filter,
            backend: None,
        }
    }

    /// PR-7b Cluster 1: wire an `EngineBackend` for filter-resolution
    /// reads. FCALL routing is cluster 2 scope.
    pub fn with_filter_and_backend(
        interval: Duration,
        lanes: Vec<LaneId>,
        partition_config: PartitionConfig,
        filter: ScannerFilter,
        backend: Arc<dyn EngineBackend>,
    ) -> Self {
        Self {
            interval,
            lanes,
            partition_config,
            filter,
            backend: Some(backend),
        }
    }
}

impl Scanner for DependencyReconciler {
    fn name(&self) -> &'static str {
        "dependency_reconciler"
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    fn filter(&self) -> &ScannerFilter {
        &self.filter
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
                if should_skip_candidate(self.backend.as_ref(), &self.filter, partition, eid_str).await {
                    continue;
                }
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
        // [8] attempt_hash, [9] stream_meta, [10] downstream_payload,
        // [11] upstream_result, [12] edgegroup (RFC-016 Stage A)
        // [10]/[11] added for Batch C item 3. [12] added for RFC-016
        // Stage A: the per-downstream edgegroup hash lives under the
        // flow partition, which is the same `{fp:N}` slot via RFC-011
        // flow-affinity co-location.
        let downstream_payload = format!("ff:exec:{}:{}:payload", tag, eid_str);
        let upstream_result = format!("ff:exec:{}:{}:result", tag, upstream_id);
        // Read the downstream's flow_id so we can construct the
        // edgegroup key. Absent / empty flow_id (standalones) means
        // there is no edge group; the key is harmless — Lua's
        // `is_set`+`EXISTS` guards skip edgegroup writes.
        let flow_id_str: Option<String> = client
            .cmd("HGET")
            .arg(&exec_core)
            .arg("flow_id")
            .execute()
            .await
            .unwrap_or_default();
        let edgegroup_key = match flow_id_str.as_deref() {
            Some(fid) if !fid.is_empty() => {
                format!("ff:flow:{}:{}:edgegroup:{}", tag, fid, eid_str)
            }
            // Standalone execution — no dep edges possible, but we
            // still need a cluster-slot-valid KEYS[12]. Use a
            // well-formed sentinel key on the same {fp:N} tag; Lua's
            // EXISTS guard skips the write.
            _ => format!("ff:flow:{}:_nil_:edgegroup:_nil_", tag),
        };
        // RFC-016 Stage C: incoming_set + pending_cancel_groups SET
        // for the sibling-enumeration + dispatcher-index path. Both
        // live on the same {fp:N} slot as the edgegroup.
        let incoming_set_key = match flow_id_str.as_deref() {
            Some(fid) if !fid.is_empty() => {
                format!("ff:flow:{}:{}:in:{}", tag, fid, eid_str)
            }
            _ => format!("ff:flow:{}:_nil_:in:_nil_", tag),
        };
        let pending_cancel_groups_key =
            format!("ff:idx:{}:pending_cancel_groups", tag);
        let flow_id_owned = flow_id_str.clone().unwrap_or_default();
        let keys: [&str; 14] = [
            &exec_core,                   // 1
            &deps_meta,                   // 2
            &deps_unresolved_key,         // 3
            &dep_key,                     // 4
            &eligible_key,                // 5
            &terminal_key,                // 6
            &blocked_deps_key,            // 7
            &attempt_hash,                // 8
            &stream_meta,                 // 9
            &downstream_payload,          // 10
            &upstream_result,             // 11
            &edgegroup_key,               // 12
            &incoming_set_key,            // 13 (RFC-016 Stage C)
            &pending_cancel_groups_key,   // 14 (RFC-016 Stage C)
        ];
        let now_s = now_ms.to_string();
        let argv: [&str; 5] = [
            edge_id.as_str(),
            resolution,
            &now_s,
            flow_id_owned.as_str(),       // 4 (RFC-016 Stage C)
            eid_str,                      // 5 (RFC-016 Stage C)
        ];

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

// ── Postgres branch (Wave 6a, RFC-v0.7) ─────────────────────────────────

/// Postgres parity for [`DependencyReconciler`].
///
/// Delegates to [`ff_backend_postgres::reconcilers::dependency::reconcile_tick`]
/// — the cascade backstop for the per-hop-tx dispatch chain in
/// Wave 5a. See that module's docs for the transitive-descendant
/// sweep proof.
///
/// The engine's Postgres scanner task drives this on a fixed
/// interval (mirrors the Valkey `ScannerRunner` contract), passing
/// its configured `ScannerFilter` and the `stale_threshold_ms`
/// from `BackendConfig`. A `None` threshold folds to
/// [`ff_backend_postgres::reconcilers::dependency::DEFAULT_STALE_THRESHOLD_MS`].
#[cfg(feature = "postgres")]
pub async fn reconcile_via_postgres(
    pool: &ff_backend_postgres::PgPool,
    filter: &ScannerFilter,
    stale_threshold_ms: Option<i64>,
) -> Result<
    ff_backend_postgres::reconcilers::dependency::ReconcileReport,
    ff_core::engine_error::EngineError,
> {
    let threshold = stale_threshold_ms.unwrap_or(
        ff_backend_postgres::reconcilers::dependency::DEFAULT_STALE_THRESHOLD_MS,
    );
    ff_backend_postgres::reconcilers::dependency::reconcile_tick(pool, filter, threshold).await
}
