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
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::Mutex as AsyncMutex;

use ff_core::keys::IndexKeys;
use ff_core::partition::{Partition, PartitionConfig, PartitionFamily, budget_partition};
use ff_core::types::{BudgetId, LaneId};

use super::{ScanResult, Scanner};

const BATCH_SIZE: u32 = 100;

/// SSCAN page size for the workers-index SET. Same COUNT the other
/// index-SET scanners use (budget_reconciler, flow_projector). Bounds
/// per-cursor round-trip response size; total iteration is still the
/// full SET size across cursor=0 wrap.
const WORKERS_SSCAN_COUNT: usize = 100;

/// Per-worker caps GET fan-out concurrency cap. Mirrors the bounded
/// parallelism W1 used in initialize_waitpoint_hmac_secret. Too low and
/// large fleets (1000+ workers) pay serial round-trip latency; too high
/// and we head-of-line the scanner client with a pipeline burst. 16 is
/// a pragmatic middle for the current fleet scales.
const CAPS_GET_CONCURRENCY: usize = 16;

pub struct UnblockScanner {
    interval: Duration,
    lanes: Vec<LaneId>,
    partition_config: PartitionConfig,
    /// Shared worker-caps union cache across ALL partitions in one scan
    /// pass. Previously this cache was declared inside `scan_partition`
    /// which runs once PER PARTITION — at 256 partitions that meant up
    /// to 256 redundant SSCAN + fan-out GET sequences per scan interval.
    /// Now: one load per TTL window (= `interval`), shared across every
    /// partition visited in that window. `TTL == interval` is natural:
    /// a worker connecting "now" propagates into the caps union on the
    /// next scan cycle, not faster or slower than the cycle itself.
    ///
    /// `Arc<AsyncMutex<_>>` because the Scanner trait's `scan_partition`
    /// takes `&self`. Contention is bounded by the partition iteration
    /// cadence (one partition at a time per scanner task), so the mutex
    /// is effectively uncontended in steady state.
    caps_cache: Arc<AsyncMutex<CapsUnionCache>>,
}

/// Worker-caps union snapshot with a monotonic freshness timestamp.
/// `None` on first scan; filled by `get_or_load`. Invalidated when
/// `Instant::now() - fetched_at >= ttl`.
struct CapsUnionCache {
    snapshot: Option<BTreeSet<String>>,
    fetched_at: Option<Instant>,
    ttl: Duration,
}

impl UnblockScanner {
    pub fn new(interval: Duration, lanes: Vec<LaneId>, partition_config: PartitionConfig) -> Self {
        Self {
            interval,
            lanes,
            partition_config,
            caps_cache: Arc::new(AsyncMutex::new(CapsUnionCache {
                snapshot: None,
                fetched_at: None,
                ttl: interval,
            })),
        }
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
            family: PartitionFamily::Flow,
            index: partition,
        };
        let idx = IndexKeys::new(&p);

        let mut total_processed: u32 = 0;
        let mut total_errors: u32 = 0;

        // Cross-partition budget cache: budget_id → is_breached.
        // Reset per partition scan (each partition scan is one "cycle").
        let mut budget_cache: HashMap<String, bool> = HashMap::new();

        // Worker-caps union cache is shared across ALL partitions via
        // the scanner struct (Arc<AsyncMutex<CapsUnionCache>>). `get_or_load`
        // returns the cached snapshot if its fetched_at is within
        // `interval` (TTL == scan interval), otherwise loads fresh via
        // SSCAN + concurrent GET fan-out. Without this, at 256 partitions
        // the old per-partition-local cache re-ran load_worker_caps_union
        // up to 256× per cycle.
        let caps_cache = self.caps_cache.clone();

        for lane in &self.lanes {
            // Scan blocked:budget
            let budget_key = idx.lane_blocked_budget(lane);
            let r = scan_blocked_set(
                client, &p, &idx, lane, &budget_key,
                "waiting_for_budget", &mut budget_cache,
                &caps_cache,
                &self.partition_config,
            ).await;
            total_processed += r.processed;
            total_errors += r.errors;

            // Scan blocked:quota
            let quota_key = idx.lane_blocked_quota(lane);
            let r = scan_blocked_set(
                client, &p, &idx, lane, &quota_key,
                "waiting_for_quota", &mut budget_cache,
                &caps_cache,
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
                &caps_cache,
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
    caps_cache: &Arc<AsyncMutex<CapsUnionCache>>,
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
                check_route_cleared(client, &core_key, caps_cache).await
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
/// The UNION of every connected worker's caps is cached on the scanner
/// struct with a TTL equal to `interval`; so within one scan cycle every
/// partition reuses the same snapshot, and between cycles a stale
/// snapshot is automatically refreshed.
///
/// Fail-OPEN on union-load failure: if the SSCAN or fan-out GET errors
/// out, assume a worker might match and let the scheduler re-decide on
/// the next tick. The caps set is non-authoritative (Lua never reads it);
/// treating it as "unknown → maybe" preserves liveness. Fail-closed
/// would leave executions stuck whenever the caps read hits a transient
/// Valkey error.
async fn check_route_cleared(
    client: &ferriskey::Client,
    core_key: &str,
    caps_cache: &Arc<AsyncMutex<CapsUnionCache>>,
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

    // Acquire the cache, return a cheap clone of the snapshot so the
    // subset check runs OUTSIDE the mutex (BTreeSet clone is O(n) but
    // n is bounded by total caps across the fleet — typically tens).
    // Holding the mutex across the subset loop would serialize every
    // partition's capability-block decision behind this one mutex.
    let snapshot: BTreeSet<String> = {
        let mut guard = caps_cache.lock().await;
        let stale = guard
            .fetched_at
            .map(|t| t.elapsed() >= guard.ttl)
            .unwrap_or(true);
        if stale {
            match load_worker_caps_union(client).await {
                Ok(union) => {
                    guard.snapshot = Some(union);
                    guard.fetched_at = Some(Instant::now());
                }
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
        guard.snapshot.clone().unwrap_or_default()
    };

    // Subset check: every non-empty token in required_csv present in union.
    required_csv
        .split(',')
        .filter(|t| !t.is_empty())
        .all(|t| snapshot.contains(t))
}

/// Union of every connected worker's advertised capabilities.
///
/// Cluster-safe enumeration pattern (matches Batch A's index SETs for
/// budget/flow/deps): SSCAN the `workers_index_key()` SET (single-slot,
/// no hash tag needed — the key name literally hashes to one slot), then
/// fan-out concurrent `GET ff:worker:<id>:caps` with a bounded concurrency
/// cap. A keyspace `SCAN MATCH ff:worker:*:caps` in cluster mode visits
/// only one shard per call and silently drops workers on other shards —
/// exactly the class of bug Batch A Issue #11 fixed.
///
/// SSCAN is used instead of SMEMBERS so a fleet of thousands of workers
/// doesn't round-trip the entire member list in one reply. `COUNT = 100`
/// matches the convention in budget_reconciler / flow_projector.
///
/// Empty caps STRING or missing key = "no caps for that worker"; scanner
/// keeps accumulating. Per-worker GET error, in contrast, PROPAGATES — a
/// previous version used `.unwrap_or(None)` which silently merged error
/// and empty into the same branch, making a transient error look like
/// "this worker has no caps". In a single-capable-worker fleet that
/// produced false-negative unions and left executions blocked even
/// though a matching worker existed, contradicting the scanner's
/// documented fail-open behavior. Now an error bubbles up; the only
/// caller (`check_route_cleared`) treats `Err` by returning `true`
/// (unblock — "we don't know, let the scheduler re-decide next tick"),
/// which preserves liveness uniformly whether the fault is SSCAN, GET,
/// or deeper transport.
async fn load_worker_caps_union(
    client: &ferriskey::Client,
) -> Result<BTreeSet<String>, ferriskey::Error> {
    let mut union = BTreeSet::new();
    let index_key = ff_core::keys::workers_index_key();

    // Helper: drain one completed future and fold its caps into the
    // union, or propagate its error. Centralizing keeps the in-loop +
    // drain paths symmetric (both must behave the same — a missed error
    // at either point re-introduces the false-negative-union bug).
    fn absorb(
        union: &mut BTreeSet<String>,
        res: Result<Option<String>, ferriskey::Error>,
    ) -> Result<(), ferriskey::Error> {
        let csv = res?;
        if let Some(csv) = csv {
            for token in csv.split(',') {
                if !token.is_empty() {
                    union.insert(token.to_owned());
                }
            }
        }
        Ok(())
    }

    // SSCAN loop — bounded per-page response size. Cursor starts at "0"
    // and wraps back to "0" when iteration completes.
    let mut cursor: String = "0".to_owned();
    loop {
        let reply: (String, Vec<String>) = client
            .cmd("SSCAN")
            .arg(&index_key)
            .arg(&cursor)
            .arg("COUNT")
            .arg(WORKERS_SSCAN_COUNT.to_string().as_str())
            .execute()
            .await?;
        cursor = reply.0;
        let worker_ids = reply.1;

        // Bounded concurrent GETs per SSCAN page. FuturesUnordered with
        // a buffered stream caps in-flight work at CAPS_GET_CONCURRENCY —
        // enough parallelism to amortize round-trip latency, bounded so
        // one scanner tick can't flood the shared Valkey client. Each
        // spawned future returns `Result<Option<String>, Error>` so
        // transient Valkey errors propagate up (no .unwrap_or(None)
        // swallowing) — see the fn-level doc for why.
        let mut pending: FuturesUnordered<_> = FuturesUnordered::new();
        for id in worker_ids {
            let client = client.clone();
            pending.push(async move {
                let caps_key = format!("ff:worker:{}:caps", id);
                let csv: Option<String> = client
                    .cmd("GET")
                    .arg(&caps_key)
                    .execute()
                    .await?;
                Ok::<Option<String>, ferriskey::Error>(csv)
            });
            if pending.len() >= CAPS_GET_CONCURRENCY
                && let Some(res) = pending.next().await
            {
                absorb(&mut union, res)?;
            }
        }
        // Drain remaining pending GETs from this page before advancing
        // the SSCAN cursor. Keeps the pipeline window bounded and the
        // union observation consistent with "all workers visible so far".
        while let Some(res) = pending.next().await {
            absorb(&mut union, res)?;
        }

        if cursor == "0" {
            break;
        }
    }
    Ok(union)
}
