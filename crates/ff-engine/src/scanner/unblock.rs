//! Unblock scanner for budget/quota-blocked executions.
//!
//! Scans `ff:idx:{p:N}:lane:<lane>:blocked:budget` and `blocked:quota`
//! per execution partition. For each blocked execution, re-evaluates the
//! blocking condition. If cleared, calls `FCALL ff_unblock_execution`.
//!
//! Cross-partition budget check is cached per scan cycle (MANDATORY —
//! without it, 50K blocked executions = 50K budget reads).
//!
//! MUST skip `paused_by_flow_cancel` — only cancel_flow clears that.
//!
//! Reference: RFC-008 §2.4, RFC-010 §6

use std::collections::HashMap;
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

        for lane in &self.lanes {
            // Scan blocked:budget
            let budget_key = idx.lane_blocked_budget(lane);
            let r = scan_blocked_set(
                client, &p, &idx, lane, &budget_key,
                "waiting_for_budget", &mut budget_cache, &self.partition_config,
            ).await;
            total_processed += r.processed;
            total_errors += r.errors;

            // Scan blocked:quota
            let quota_key = idx.lane_blocked_quota(lane);
            let r = scan_blocked_set(
                client, &p, &idx, lane, &quota_key,
                "waiting_for_quota", &mut budget_cache, &self.partition_config,
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
async fn scan_blocked_set(
    client: &ferriskey::Client,
    partition: &Partition,
    idx: &IndexKeys,
    lane: &LaneId,
    blocked_key: &str,
    expected_reason: &str,
    budget_cache: &mut HashMap<String, bool>,
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
            Err(_) => continue,
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
            Err(_) => continue,
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

    // Read hard limits
    let limits: Vec<String> = client
        .cmd("HGETALL")
        .arg(&limits_key)
        .execute()
        .await
        .unwrap_or_default();

    for i in (0..limits.len()).step_by(2) {
        let field = &limits[i];
        let limit_str = &limits[i + 1];

        if !field.starts_with("hard:") {
            continue;
        }
        let dimension = &field[5..];
        let limit: u64 = match limit_str.parse() {
            Ok(v) if v > 0 => v,
            _ => continue,
        };

        let usage_str: Option<String> = client
            .cmd("HGET")
            .arg(&usage_key)
            .arg(dimension)
            .execute()
            .await
            .unwrap_or(None);
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
    // Read quota_policy_id from exec_core
    let quota_id: Option<String> = client
        .cmd("HGET")
        .arg(core_key)
        .arg("quota_policy_id")
        .execute()
        .await
        .unwrap_or(None);

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

    let rate_limit: u64 = client
        .cmd("HGET").arg(&quota_def_key).arg("requests_per_window_limit")
        .execute().await.ok().flatten()
        .and_then(|s: String| s.parse().ok())
        .unwrap_or(0);
    let window_secs: u64 = client
        .cmd("HGET").arg(&quota_def_key).arg("requests_per_window_seconds")
        .execute().await.ok().flatten()
        .and_then(|s: String| s.parse().ok())
        .unwrap_or(60);
    let concurrency_cap: u64 = client
        .cmd("HGET").arg(&quota_def_key).arg("active_concurrency_cap")
        .execute().await.ok().flatten()
        .and_then(|s: String| s.parse().ok())
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
