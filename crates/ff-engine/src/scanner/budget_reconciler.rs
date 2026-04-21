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

        // Discover budgets via partition-level index SET (cluster-safe).
        // Stream in SSCAN batches so partitions with many budgets do not
        // materialise the entire id list into one allocation and do not
        // hold Valkey's single-threaded SMEMBERS for a large set.
        let policies_key = keys::budget_policies_index(&tag);
        let mut processed: u32 = 0;
        let mut errors: u32 = 0;
        let mut cursor = "0".to_string();

        loop {
            let result: ferriskey::Value = match client
                .cmd("SSCAN")
                .arg(&policies_key)
                .arg(cursor.as_str())
                .arg("COUNT")
                .arg("100")
                .execute()
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(partition, error = %e, "budget_reconciler: SSCAN failed");
                    return ScanResult { processed, errors: errors + 1 };
                }
            };

            let (next_cursor, budget_ids) = parse_sscan_response(&result);

            for bid in &budget_ids {
                match reconcile_one_budget(client, &tag, &policies_key, bid, now_ms).await {
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

            cursor = next_cursor;
            if cursor == "0" {
                break;
            }
        }

        ScanResult { processed, errors }
    }
}

/// Reconcile one budget. Returns Ok(true) if corrected, Ok(false) if skipped.
async fn reconcile_one_budget(
    client: &ferriskey::Client,
    tag: &str,
    policies_key: &str,
    budget_id: &str,
    now_ms: u64,
) -> Result<bool, ferriskey::Error> {
    let def_key = format!("ff:budget:{}:{}", tag, budget_id);
    let usage_key = format!("ff:budget:{}:{}:usage", tag, budget_id);
    let limits_key = format!("ff:budget:{}:{}:limits", tag, budget_id);

    // Defensive prune: index entry for a budget whose definition is gone
    // (manual delete / retention purge) — drop it so SMEMBERS stays correct.
    //
    // Fail-closed on transport errors: we MUST distinguish "key absent"
    // (empty Vec) from "read failed" (Err). Pre-fix the unwrap_or_default
    // collapsed them, so a transient WRONGTYPE / IoError / ClusterDown
    // on the def key would SREM the budget from the partition index —
    // durable metadata loss triggered by a momentary Valkey blip.
    let def_raw: Vec<String> = client
        .cmd("HGETALL")
        .arg(&def_key)
        .execute()
        .await?;
    if def_raw.is_empty() {
        let _: Option<i64> = client
            .cmd("SREM")
            .arg(policies_key)
            .arg(budget_id)
            .execute()
            .await
            .unwrap_or(None);
        return Ok(false);
    }

    let def_map = pairs_to_map(&def_raw);
    let reset_interval = def_map.get("reset_interval_ms").copied();

    // Skip resetting budgets — cannot reconcile by summing all-time usage
    if let Some(ri) = reset_interval
        && !ri.is_empty()
        && ri != "0"
    {
        return Ok(false);
    }

    // Read current usage counters. Fail-closed on transport errors
    // (same reasoning as def_key above): an empty Vec from a WRONGTYPE
    // would make the subsequent breach comparison compute against zero
    // and silently clear the `breached_at` marker below.
    let usage_raw: Vec<String> = client
        .cmd("HGETALL")
        .arg(&usage_key)
        .execute()
        .await?;

    // Read hard limits (same fail-closed treatment).
    let limits_raw: Vec<String> = client
        .cmd("HGETALL")
        .arg(&limits_key)
        .execute()
        .await?;

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

    // TODO: cross-partition stale-execution cleanup for ff:budget:{b:M}:<id>:executions.
    // Retention on {p:N} can't reach this {b:M} reverse index, so entries
    // accumulate after executions are purged. Needs a partition-aware EXISTS
    // check (parse UUID → execution_partition(config) → core key) — deferred
    // to a later pass since it requires PartitionConfig plumbing here.

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

/// Parse an SSCAN reply `[cursor, [member1, member2, ...]]` into
/// `(cursor, Vec<member>)`. Mirrors the helper in quota_reconciler /
/// flow_projector so all three scanners agree on the wire shape.
fn parse_sscan_response(val: &ferriskey::Value) -> (String, Vec<String>) {
    let arr = match val {
        ferriskey::Value::Array(a) if a.len() >= 2 => a,
        _ => return ("0".to_string(), vec![]),
    };

    let cursor = match &arr[0] {
        Ok(ferriskey::Value::BulkString(b)) => String::from_utf8_lossy(b).into_owned(),
        Ok(ferriskey::Value::SimpleString(s)) => s.clone(),
        _ => return ("0".to_string(), vec![]),
    };

    let mut members = Vec::new();
    match &arr[1] {
        Ok(ferriskey::Value::Array(inner)) => {
            for item in inner {
                if let Ok(ferriskey::Value::BulkString(b)) = item {
                    members.push(String::from_utf8_lossy(b).into_owned());
                }
            }
        }
        Ok(ferriskey::Value::Set(inner)) => {
            for item in inner {
                if let ferriskey::Value::BulkString(b) = item {
                    members.push(String::from_utf8_lossy(b).into_owned());
                }
            }
        }
        _ => {}
    }

    (cursor, members)
}

