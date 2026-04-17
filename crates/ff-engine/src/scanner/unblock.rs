//! Unblock scanner for budget/quota/capability-blocked executions.
//!
//! Scans `ff:idx:{p:N}:lane:<lane>:blocked:{budget,quota,route}` per
//! execution partition. For each blocked execution, re-evaluates the
//! blocking condition. If cleared, calls `FCALL ff_unblock_execution`.
//!
//! Cross-partition budget check is cached per scan cycle (MANDATORY —
//! without it, 50K blocked executions = 50K budget reads).
//!
//! Capability sweep reads the union of non-authoritative worker cap sets
//! (`ff:worker:*:caps` — written by `ff-sdk::FlowFabricWorker::connect`)
//! ONCE per scan cycle and uses it to decide whether a `waiting_for_capable_worker`
//! execution has a matching worker. This is best-effort: caps sets may
//! be slightly stale (TTL-less STRING, overwrite on restart), but the
//! promotion path is self-correcting — a promoted execution that still
//! can't be claimed gets re-blocked on the next scheduler tick. RFC-009
//! §7.5 documents the v1 sweep approach and defers connect-triggered
//! sweeps to V2.
//!
//! MUST skip `paused_by_flow_cancel` — only cancel_flow clears that.
//!
//! Reference: RFC-008 §2.4, RFC-009 §7.5, RFC-010 §6

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use ff_core::keys::IndexKeys;
use ff_core::partition::{Partition, PartitionConfig, PartitionFamily, budget_partition};
use ff_core::types::{BudgetId, LaneId};

use super::{ScanResult, Scanner};

const BATCH_SIZE: u32 = 100;

pub struct UnblockScanner {
    interval: Duration,
    lanes: Vec<LaneId>,
    partition_config: PartitionConfig,
}

impl UnblockScanner {
    pub fn new(interval: Duration, lanes: Vec<LaneId>, partition_config: PartitionConfig) -> Self {
        Self { interval, lanes, partition_config }
    }
}

impl Scanner for UnblockScanner {
    fn name(&self) -> &'static str {
        "unblock"
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

        let mut total_processed: u32 = 0;
        let mut total_errors: u32 = 0;

        // Cross-partition budget cache: budget_id → is_breached.
        // Reset per partition scan (each partition scan is one "cycle").
        let mut budget_cache: HashMap<String, bool> = HashMap::new();

        // Worker-caps union, read ONCE per partition scan.
        // None = "not queried yet on this cycle"; Some(set) = cached result.
        // Empty set means no online workers have any caps → nothing to promote.
        // Partitions scanned later in a cycle reuse the same cached union;
        // load is bounded by SCAN + GET per `ff:worker:*:caps` key per cycle.
        let mut caps_union_cache: Option<BTreeSet<String>> = None;

        for lane in &self.lanes {
            // Scan blocked:budget
            let budget_key = idx.lane_blocked_budget(lane);
            let r = scan_blocked_set(
                client, &p, &idx, lane, &budget_key,
                "waiting_for_budget", &mut budget_cache,
                &mut caps_union_cache,
                &self.partition_config,
            ).await;
            total_processed += r.processed;
            total_errors += r.errors;

            // Scan blocked:quota
            let quota_key = idx.lane_blocked_quota(lane);
            let r = scan_blocked_set(
                client, &p, &idx, lane, &quota_key,
                "waiting_for_quota", &mut budget_cache,
                &mut caps_union_cache,
                &self.partition_config,
            ).await;
            total_processed += r.processed;
            total_errors += r.errors;

            // Scan blocked:route (capability-mismatch blocks). Promotion
            // decision reads the union of connected workers' caps and
            // checks subset coverage. See check_route_cleared below.
            let route_key = idx.lane_blocked_route(lane);
            let r = scan_blocked_set(
                client, &p, &idx, lane, &route_key,
                "waiting_for_capable_worker", &mut budget_cache,
                &mut caps_union_cache,
                &self.partition_config,
            ).await;
            total_processed += r.processed;
            total_errors += r.errors;
        }

        ScanResult {
            processed: total_processed,
            errors: total_errors,
        }
    }
}

/// Scan one blocked set and unblock executions whose condition has cleared.
#[allow(clippy::too_many_arguments)]
async fn scan_blocked_set(
    client: &ferriskey::Client,
    partition: &Partition,
    idx: &IndexKeys,
    lane: &LaneId,
    blocked_key: &str,
    expected_reason: &str,
    budget_cache: &mut HashMap<String, bool>,
    caps_union_cache: &mut Option<BTreeSet<String>>,
    partition_config: &PartitionConfig,
) -> ScanResult {
    // Read all members from the blocked set (they're scored by block time)
    let blocked: Vec<String> = match client
        .cmd("ZRANGEBYSCORE")
        .arg(blocked_key)
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
                error = %e,
                blocked_key,
                "unblock_scanner: ZRANGEBYSCORE failed"
            );
            return ScanResult { processed: 0, errors: 1 };
        }
    };

    if blocked.is_empty() {
        return ScanResult { processed: 0, errors: 0 };
    }

    let mut processed: u32 = 0;
    let mut errors: u32 = 0;
    let tag = partition.hash_tag();

    for eid_str in &blocked {
        // Read blocking_reason from exec_core to confirm still blocked
        let core_key = format!("ff:exec:{}:{}:core", tag, eid_str);
        let reason: Option<String> = match client
            .cmd("HGET")
            .arg(&core_key)
            .arg("blocking_reason")
            .execute()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    execution_id = eid_str.as_str(),
                    error = %e,
                    "unblock_scanner: HGET blocking_reason failed, skipping"
                );
                errors += 1;
                continue;
            }
        };

        let reason = reason.unwrap_or_default();

        // Skip if not blocked by the expected reason (e.g. paused_by_flow_cancel)
        if reason != expected_reason {
            continue;
        }

        // Re-evaluate the blocking condition
        let should_unblock = match expected_reason {
            "waiting_for_budget" => {
                check_budget_cleared(client, &core_key, budget_cache, partition_config).await
            }
            "waiting_for_quota" => {
                check_quota_cleared(client, &core_key, eid_str, partition_config).await
            }
            "waiting_for_capable_worker" => {
                check_route_cleared(client, &core_key, caps_union_cache).await
            }
            _ => false,
        };

        if !should_unblock {
            continue;
        }

        // Unblock: FCALL ff_unblock_execution on {p:N}
        let eligible_key = idx.lane_eligible(lane);
        let keys: [&str; 3] = [&core_key, blocked_key, &eligible_key];

        let now_ms = match crate::scanner::lease_expiry::server_time_ms(client).await {
            Ok(t) => t.to_string(),
            Err(e) => {
                tracing::warn!(
                    execution_id = eid_str.as_str(),
                    error = %e,
                    "unblock_scanner: server TIME failed, skipping unblock"
                );
                errors += 1;
                continue;
            }
        };
        let argv: [&str; 3] = [eid_str, &now_ms, expected_reason];

        match client
            .fcall::<ferriskey::Value>("ff_unblock_execution", &keys, &argv)
            .await
        {
            Ok(_) => {
                tracing::info!(
                    execution_id = eid_str.as_str(),
                    reason = expected_reason,
                    "unblock_scanner: execution unblocked"
                );
                processed += 1;
            }
            Err(e) => {
                tracing::warn!(
                    execution_id = eid_str.as_str(),
                    error = %e,
                    "unblock_scanner: ff_unblock_execution failed"
                );
                errors += 1;
            }
        }
    }

    ScanResult { processed, errors }
}

/// Check if budget blocking condition has cleared.
/// Uses cross-partition cache to avoid redundant reads.
async fn check_budget_cleared(
    client: &ferriskey::Client,
    core_key: &str,
    cache: &mut HashMap<String, bool>,
    config: &PartitionConfig,
) -> bool {
    // Read budget_ids from exec_core
    let budget_ids_str: Option<String> = client
        .cmd("HGET")
        .arg(core_key)
        .arg("budget_ids")
        .execute()
        .await
        .unwrap_or(None);

    let budget_ids_str = match budget_ids_str {
        Some(s) if !s.is_empty() => s,
        _ => return true, // no budgets → unblock
    };

    for budget_id in budget_ids_str.split(',') {
        let budget_id = budget_id.trim();
        if budget_id.is_empty() {
            continue;
        }

        // Check cache first
        if let Some(&breached) = cache.get(budget_id) {
            if breached {
                return false; // still breached
            }
            continue;
        }

        // Read from Valkey and cache
        let breached = is_budget_breached(client, budget_id, config).await;
        cache.insert(budget_id.to_owned(), breached);
        if breached {
            return false;
        }
    }

    true // all budgets within limits
}

/// Read budget usage + limits and check if any hard limit is breached.
/// Computes real {b:M} partition tag from budget_id.
async fn is_budget_breached(
    client: &ferriskey::Client,
    budget_id: &str,
    config: &PartitionConfig,
) -> bool {
    // Compute the real budget partition tag
    let bid = match BudgetId::parse(budget_id) {
        Ok(id) => id,
        Err(_) => return false, // invalid budget_id → treat as not breached
    };
    let partition = budget_partition(&bid, config);
    let tag = partition.hash_tag();
    let usage_key = format!("ff:budget:{}:{}:usage", tag, budget_id);
    let limits_key = format!("ff:budget:{}:{}:limits", tag, budget_id);

    // Read hard limits — fail-closed: if Valkey read fails, treat as breached
    // (keep execution blocked) rather than silently unblocking
    let limits: Vec<String> = match client
        .cmd("HGETALL")
        .arg(&limits_key)
        .execute()
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(
                budget_id,
                error = %e,
                "unblock_scanner: budget limits read failed, keeping blocked (fail-closed)"
            );
            return true; // treat as breached
        }
    };

    let mut i = 0;
    while i + 1 < limits.len() {
        let field = &limits[i];
        let limit_str = &limits[i + 1];
        i += 2;

        if !field.starts_with("hard:") {
            continue;
        }
        let dimension = &field[5..];
        let limit: u64 = match limit_str.parse() {
            Ok(v) if v > 0 => v,
            _ => continue,
        };

        let usage_str: Option<String> = match client
            .cmd("HGET")
            .arg(&usage_key)
            .arg(dimension)
            .execute()
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(
                    budget_id,
                    dimension,
                    error = %e,
                    "unblock_scanner: budget usage read failed, keeping blocked (fail-closed)"
                );
                return true; // treat as breached
            }
        };
        let usage: u64 = usage_str.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0);

        if usage >= limit {
            return true; // breached
        }
    }

    false
}

/// Check if quota blocking condition has cleared.
/// Re-checks the sliding window after cleanup.
/// Computes real {q:K} partition tag from quota_policy_id.
async fn check_quota_cleared(
    client: &ferriskey::Client,
    core_key: &str,
    _eid_str: &str,
    config: &PartitionConfig,
) -> bool {
    // Read quota_policy_id from exec_core — fail-closed on Valkey error
    let quota_id: Option<String> = match client
        .cmd("HGET")
        .arg(core_key)
        .arg("quota_policy_id")
        .execute()
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(
                core_key,
                error = %e,
                "unblock_scanner: quota_policy_id read failed, keeping blocked (fail-closed)"
            );
            return false;
        }
    };

    let quota_id = match quota_id {
        Some(s) if !s.is_empty() => s,
        _ => return true, // no quota → unblock
    };

    // Compute real quota partition tag
    let qid = match ff_core::types::QuotaPolicyId::parse(&quota_id) {
        Ok(id) => id,
        Err(_) => return true, // invalid → unblock (advisory)
    };
    let partition = ff_core::partition::quota_partition(&qid, config);
    let tag = partition.hash_tag();

    let quota_def_key = format!("ff:quota:{}:{}", tag, quota_id);
    let window_key = format!("ff:quota:{}:{}:window:requests_per_window", tag, quota_id);
    let concurrency_key = format!("ff:quota:{}:{}:concurrency", tag, quota_id);

    // Read quota definition fields — fail-closed on Valkey error
    let def_fields: Vec<Option<String>> = match client
        .cmd("HMGET")
        .arg(&quota_def_key)
        .arg("max_requests_per_window")
        .arg("requests_per_window_seconds")
        .arg("active_concurrency_cap")
        .execute()
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(
                quota_id = %quota_id,
                error = %e,
                "unblock_scanner: quota definition read failed, keeping blocked (fail-closed)"
            );
            return false;
        }
    };
    let rate_limit: u64 = def_fields.first()
        .and_then(|v| v.as_ref())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let window_secs: u64 = def_fields.get(1)
        .and_then(|v| v.as_ref())
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let concurrency_cap: u64 = def_fields.get(2)
        .and_then(|v| v.as_ref())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    // Check rate: clean window, count
    if rate_limit > 0 {
        let now_ms = match crate::scanner::lease_expiry::server_time_ms(client).await {
            Ok(t) => t,
            Err(_) => return false,
        };
        let window_ms = window_secs * 1000;
        let cutoff = (now_ms.saturating_sub(window_ms)).to_string();

        let _: Result<i64, _> = client
            .cmd("ZREMRANGEBYSCORE")
            .arg(&window_key)
            .arg("-inf")
            .arg(&cutoff)
            .execute()
            .await;

        let count: i64 = client
            .cmd("ZCARD")
            .arg(&window_key)
            .execute()
            .await
            .unwrap_or(0);

        if count as u64 >= rate_limit {
            return false; // still at limit
        }
    }

    // Check concurrency
    if concurrency_cap > 0 {
        let active: i64 = client
            .cmd("GET")
            .arg(&concurrency_key)
            .execute()
            .await
            .unwrap_or(0);

        if active as u64 >= concurrency_cap {
            return false; // still at cap
        }
    }

    true // quota cleared
}

/// Check if the capability-block has cleared: some connected worker's
/// caps now cover the execution's `required_capabilities`.
///
/// v1: compute the UNION of every connected worker's caps once per scan
/// cycle (cached in `caps_union_cache`). If `required_capabilities ⊆ union`,
/// unblock — the execution has at least one worker that *could* claim it
/// on the next scheduling tick. If the worker is unavailable or the
/// scheduler re-mismatches, the scheduler will block it again. That's
/// the RFC-009 §7.5 "slow cycle" approximation; connect-triggered sweeps
/// are V2.
///
/// Fail-OPEN on union-load failure: if `ff:worker:*:caps` read errors
/// out, assume a worker might match and let the scheduler re-decide on
/// the next tick. The caps set is non-authoritative (Lua never reads it);
/// treating it as "unknown → maybe" preserves liveness. The alternative
/// (fail-closed) would leave executions stuck whenever the caps-SCAN
/// happens to hit a transient Valkey error.
async fn check_route_cleared(
    client: &ferriskey::Client,
    core_key: &str,
    caps_union_cache: &mut Option<BTreeSet<String>>,
) -> bool {
    let required_csv: Option<String> = client
        .cmd("HGET")
        .arg(core_key)
        .arg("required_capabilities")
        .execute()
        .await
        .unwrap_or(None);
    let required_csv = match required_csv {
        Some(s) if !s.is_empty() => s,
        _ => return true, // no required caps → anyone can claim
    };

    if caps_union_cache.is_none() {
        match load_worker_caps_union(client).await {
            Ok(union) => *caps_union_cache = Some(union),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "unblock_scanner: failed to read worker caps union — \
                     assuming match possible (fail-open to preserve liveness)"
                );
                return true;
            }
        }
    }
    let union = caps_union_cache.as_ref().expect("cache populated above");

    // Subset check: every non-empty token in required_csv present in union.
    required_csv
        .split(',')
        .filter(|t| !t.is_empty())
        .all(|t| union.contains(t))
}

/// Union of every connected worker's advertised capabilities.
///
/// Cluster-safe enumeration: reads the members of
/// `workers_index_key()` (a SET of worker_instance_ids maintained by
/// `ff-sdk::FlowFabricWorker::connect`), then per-member `GET` on the
/// `ff:worker:<id>:caps` STRING. A naive `SCAN MATCH ff:worker:*:caps`
/// would silently miss workers whose keys hash to shards the `SCAN`
/// command doesn't visit — Valkey cluster `SCAN` is per-node, not
/// cluster-wide. This is the same class of bug Batch A fixed by
/// promoting `budget_policies_index` / `flow_index` / `deps_all_edges`
/// from keyspace SCAN to explicit index SETs.
///
/// Empty caps STRING, missing key, or per-worker GET error are treated
/// as "no caps" for that worker — the scanner keeps going and the
/// scheduler will re-evaluate on the next tick anyway (fail-open
/// matches check_route_cleared's overall policy).
async fn load_worker_caps_union(
    client: &ferriskey::Client,
) -> Result<BTreeSet<String>, ferriskey::Error> {
    let mut union = BTreeSet::new();
    let index_key = ff_core::keys::workers_index_key();

    // Single-slot SMEMBERS: index key has no hash tag, so every node
    // routes to the same slot. Bounded by the number of connected
    // workers, which is the authoritative universe — unlike
    // `SCAN MATCH ff:worker:*:caps` which in cluster mode only hits one
    // shard's keyspace per call.
    let worker_ids: Vec<String> = client
        .cmd("SMEMBERS")
        .arg(&index_key)
        .execute()
        .await?;

    for id in &worker_ids {
        let caps_key = format!("ff:worker:{}:caps", id);
        let csv: Option<String> = client
            .cmd("GET")
            .arg(&caps_key)
            .execute()
            .await
            .unwrap_or(None);
        if let Some(csv) = csv {
            for token in csv.split(',') {
                if !token.is_empty() {
                    union.insert(token.to_owned());
                }
            }
        }
    }
    Ok(union)
}
