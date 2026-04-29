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
use ff_core::contracts::ResolveDependencyArgs;
use ff_core::engine_backend::EngineBackend;
use ff_core::keys::IndexKeys;
use ff_core::partition::{Partition, PartitionConfig, PartitionFamily, execution_partition};
use ff_core::types::{AttemptIndex, EdgeId, ExecutionId, FlowId, LaneId, TimestampMs};

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
                    client, self.backend.as_ref(), &p, &tag, &idx, lane, eid_str,
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
#[allow(clippy::too_many_arguments)]
async fn reconcile_one_execution(
    client: &ferriskey::Client,
    backend: Option<&Arc<dyn EngineBackend>>,
    partition: &Partition,
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

        // Read current_attempt_index + flow_id (needed for both the
        // trait-routed and the fallback paths — Valkey's impl reads
        // these itself from `ResolveDependencyArgs`).
        let exec_core = format!("ff:exec:{}:{}:core", tag, eid_str);
        let att_idx_str: Option<String> = client
            .cmd("HGET")
            .arg(&exec_core)
            .arg("current_attempt_index")
            .execute()
            .await?;
        let att_idx_n: u32 = att_idx_str.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0);
        let flow_id_str: Option<String> = client
            .cmd("HGET")
            .arg(&exec_core)
            .arg("flow_id")
            .execute()
            .await
            .unwrap_or_default();

        if let Some(backend_arc) = backend {
            // PR-7b Cluster 2: trait-routed resolve. The Valkey impl
            // wraps `ff_resolve_dependency` with identical KEYS[14] /
            // ARGV[5]; Postgres returns `Unavailable` (structural —
            // PG's dispatch uses `dispatch_completion(event_id)` per
            // `resolve_dependency` rustdoc).
            let Ok(downstream_eid) = ExecutionId::parse(eid_str) else {
                tracing::warn!(execution_id = eid_str, "malformed eid; skipping");
                continue;
            };
            let Ok(upstream_eid) = ExecutionId::parse(&upstream_id) else {
                tracing::warn!(upstream_id = %upstream_id, "malformed upstream eid; skipping");
                continue;
            };
            let flow_id_parsed = flow_id_str
                .as_deref()
                .and_then(|s| if s.is_empty() { None } else { FlowId::parse(s).ok() });
            // `resolve_dependency` requires a flow_id; standalone
            // executions have no flow deps so skip — the Lua path's
            // `_nil_` sentinel only works when the FCALL is issued
            // anyway with a matching slot key. The trait surface uses
            // a typed `FlowId` (no sentinel); defer standalones to the
            // fallback path below.
            let Some(flow_id) = flow_id_parsed else {
                // No flow → no dep edges. Shouldn't happen here since
                // we've just read `deps:unresolved` for this exec, but
                // be defensive — skip silently rather than fabricate a
                // nil flow id.
                continue;
            };
            let Ok(edge_id_parsed) = EdgeId::parse(edge_id) else {
                tracing::warn!(edge_id = edge_id.as_str(), "malformed edge_id; skipping");
                continue;
            };
            let args = ResolveDependencyArgs::new(
                *partition,
                flow_id,
                downstream_eid,
                upstream_eid,
                edge_id_parsed,
                lane.clone(),
                AttemptIndex::new(att_idx_n),
                terminal_outcome.clone(),
                TimestampMs::from_millis(now_ms as i64),
            );
            match backend_arc.resolve_dependency(args).await {
                Ok(outcome) => {
                    resolved += 1;
                    tracing::debug!(
                        execution_id = eid_str,
                        edge_id = edge_id.as_str(),
                        upstream_id = upstream_id.as_str(),
                        resolution = resolution,
                        ?outcome,
                        "dependency_reconciler: resolved stale dependency"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        execution_id = eid_str,
                        edge_id = edge_id.as_str(),
                        error = %e,
                        "dependency_reconciler: resolve_dependency failed"
                    );
                }
            }
            continue;
        }

        // ── Test-only fallback (no backend wired) ──
        // Preserves the pre-trait-routing FCALL path for unit tests that
        // construct the scanner without a backend. Mirrors cluster-1's
        // lease_expiry pattern.
        let deps_meta = format!("ff:exec:{}:{}:deps:meta", tag, eid_str);
        let eligible_key = idx.lane_eligible(lane);
        let blocked_deps_key = idx.lane_blocked_dependencies(lane);
        let terminal_key = idx.lane_terminal(lane);

        let att_idx = att_idx_str.as_deref().unwrap_or("0");
        let attempt_hash = format!("ff:attempt:{}:{}:{}", tag, eid_str, att_idx);
        let stream_meta = format!("ff:stream:{}:{}:{}:meta", tag, eid_str, att_idx);

        let downstream_payload = format!("ff:exec:{}:{}:payload", tag, eid_str);
        let upstream_result = format!("ff:exec:{}:{}:result", tag, upstream_id);
        let edgegroup_key = match flow_id_str.as_deref() {
            Some(fid) if !fid.is_empty() => {
                format!("ff:flow:{}:{}:edgegroup:{}", tag, fid, eid_str)
            }
            _ => format!("ff:flow:{}:_nil_:edgegroup:_nil_", tag),
        };
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
            &exec_core,
            &deps_meta,
            &deps_unresolved_key,
            &dep_key,
            &eligible_key,
            &terminal_key,
            &blocked_deps_key,
            &attempt_hash,
            &stream_meta,
            &downstream_payload,
            &upstream_result,
            &edgegroup_key,
            &incoming_set_key,
            &pending_cancel_groups_key,
        ];
        let now_s = now_ms.to_string();
        let argv: [&str; 5] = [
            edge_id.as_str(),
            resolution,
            &now_s,
            flow_id_owned.as_str(),
            eid_str,
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
                    "dependency_reconciler: resolved stale dependency (fallback)"
                );
            }
            Err(e) => {
                tracing::warn!(
                    execution_id = eid_str,
                    edge_id = edge_id.as_str(),
                    error = %e,
                    "dependency_reconciler: ff_resolve_dependency failed (fallback)"
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
