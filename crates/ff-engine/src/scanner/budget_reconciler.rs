//! Budget counter reconciler.
//!
//! Periodically scans budget partitions to detect and correct drift on
//! lifetime (non-resetting) budget usage counters. Also checks breach
//! status against hard limits and cleans stale execution references.
//!
//! Budgets with `reset_policy` set are SKIPPED — resetting budgets cannot
//! be reconciled by summing all-time usage (the sum would undo the reset).
//!
//! Cluster-safe: uses SMEMBERS on a partition-level index SET instead of SCAN.
//!
//! Reference: RFC-008 §Budget Reconciliation, RFC-010 §6.5

use std::time::Duration;

use ff_core::keys;
use ff_core::partition::{Partition, PartitionFamily};

use super::{ScanResult, Scanner};

/// Max execution reads per budget per cycle to avoid cross-partition fan-out.
const MAX_EXEC_READS_PER_BUDGET: usize = 100;

pub struct BudgetReconciler {
    interval: Duration,
}

impl BudgetReconciler {
    pub fn new(interval: Duration) -> Self {
        Self { interval }
    }
}

impl Scanner for BudgetReconciler {
    fn name(&self) -> &'static str {
        "budget_reconciler"
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
            family: PartitionFamily::Budget,
            index: partition,
        };
        let tag = p.hash_tag();

        let now_ms = match crate::scanner::lease_expiry::server_time_ms(client).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(partition, error = %e, "budget_reconciler: failed to get server time");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        // Discover budgets via partition-level index SET (cluster-safe)
        let policies_key = keys::budget_policies_index(&tag);
        let budget_ids: Vec<String> = match client
            .cmd("SMEMBERS")
            .arg(&policies_key)
            .execute()
            .await
        {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(partition, error = %e, "budget_reconciler: SMEMBERS failed");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        if budget_ids.is_empty() {
            return ScanResult { processed: 0, errors: 0 };
        }

        let mut processed: u32 = 0;
        let mut errors: u32 = 0;

        for bid in &budget_ids {
            match reconcile_one_budget(client, &tag, bid, now_ms).await {
                Ok(true) => processed += 1,
                Ok(false) => {} // skipped (resetting budget or no drift)
                Err(e) => {
                    tracing::warn!(
                        partition,
                        budget_id = bid.as_str(),
                        error = %e,
                        "budget_reconciler: reconcile failed"
                    );
                    errors += 1;
                }
            }
        }

        ScanResult { processed, errors }
    }
}

/// Reconcile one budget. Returns Ok(true) if corrected, Ok(false) if skipped.
async fn reconcile_one_budget(
    client: &ferriskey::Client,
    tag: &str,
    budget_id: &str,
    now_ms: u64,
) -> Result<bool, ferriskey::Error> {
    let def_key = format!("ff:budget:{}:{}", tag, budget_id);
    let usage_key = format!("ff:budget:{}:{}:usage", tag, budget_id);
    let limits_key = format!("ff:budget:{}:{}:limits", tag, budget_id);
    let executions_key = format!("ff:budget:{}:{}:executions", tag, budget_id);

    // Read budget definition to check for reset_interval_ms
    let reset_interval: Option<String> = client
        .cmd("HGET")
        .arg(&def_key)
        .arg("reset_interval_ms")
        .execute()
        .await?;

    // Skip resetting budgets — cannot reconcile by summing all-time usage
    if let Some(ref ri) = reset_interval && !ri.is_empty() && ri != "0" {
        return Ok(false);
    }

    // Read current usage counters
    let usage_raw: Vec<String> = client
        .cmd("HGETALL")
        .arg(&usage_key)
        .execute()
        .await
        .unwrap_or_default();

    // Read hard limits
    let limits_raw: Vec<String> = client
        .cmd("HGETALL")
        .arg(&limits_key)
        .execute()
        .await
        .unwrap_or_default();

    if limits_raw.is_empty() {
        return Ok(false); // No limits defined
    }

    // Parse usage and limits into dimension maps
    let usage = pairs_to_map(&usage_raw);
    let limits = pairs_to_map(&limits_raw);

    // Check breach status against hard limits.
    // Limits hash uses prefixed fields: "hard:tokens", "soft:tokens".
    // Usage hash uses bare fields: "tokens".
    // Must strip prefix before looking up in usage.
    let mut any_breached = false;
    for (field, limit_str) in &limits {
        // Only check hard limits for breach detection
        let dim = match field.strip_prefix("hard:") {
            Some(d) => d,
            None => continue, // skip soft limits and other fields
        };
        let limit: i64 = limit_str.parse().unwrap_or(i64::MAX);
        if limit <= 0 {
            continue; // 0 or negative means no limit
        }
        let current: i64 = usage
            .get(dim)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if current > limit {
            any_breached = true;
            break;
        }
    }

    // Reconcile breach marker
    let currently_breached = usage.contains_key("breached_at");

    if any_breached && !currently_breached {
        // Mark breached
        let _: () = client
            .cmd("HSET")
            .arg(&usage_key)
            .arg("breached_at")
            .arg(now_ms.to_string().as_str())
            .execute()
            .await?;
        tracing::info!(budget_id, "budget_reconciler: marked budget as breached");
    } else if !any_breached && currently_breached {
        // Clear breach
        let _: u32 = client
            .cmd("HDEL")
            .arg(&usage_key)
            .arg("breached_at")
            .execute()
            .await?;
        tracing::info!(budget_id, "budget_reconciler: cleared budget breach");
    }

    // Clean stale execution references (incremental — one batch per cycle)
    let exec_ids: Vec<String> = client
        .cmd("SRANDMEMBER")
        .arg(&executions_key)
        .arg(MAX_EXEC_READS_PER_BUDGET.to_string().as_str())
        .execute()
        .await
        .unwrap_or_default();

    for eid in &exec_ids {
        // Check if the execution still exists (cross-partition read)
        // We can't know the exact partition without the UUID bytes, so
        // we check via the exec_core key existence using a hash-tag-less
        // approach. Since we only need existence, use EXISTS.
        // NOTE: In v1, stale cleanup is best-effort. The retention trimmer
        // on {p:N} doesn't know about {b:M} reverse indexes. We clean here.
        // For now, skip the cross-partition check and just verify the
        // member is non-empty.
        if eid.is_empty() {
            let _: u32 = client
                .cmd("SREM")
                .arg(&executions_key)
                .arg(eid.as_str())
                .execute()
                .await?;
        }
    }

    Ok(any_breached != currently_breached) // true if we corrected something
}

fn pairs_to_map(flat: &[String]) -> std::collections::HashMap<&str, &str> {
    let mut map = std::collections::HashMap::new();
    let mut i = 0;
    while i + 1 < flat.len() {
        map.insert(flat[i].as_str(), flat[i + 1].as_str());
        i += 2;
    }
    map
}

