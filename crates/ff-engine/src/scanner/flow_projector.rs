//! Flow summary projector scanner.
//!
//! Scans flow partitions ({fp:N}). For each flow: reads member executions
//! (SMEMBERS), reads each member's public_state (cross-partition), and
//! updates the flow summary hash with aggregate counts and derived
//! public_flow_state.
//!
//! Interval: 15s (catchup mode — event-driven in production).
//!
//! Reference: RFC-007 §Flow Summary Projection, RFC-010 §6.7

use std::collections::HashMap;
use std::time::Duration;

use ff_core::partition::{Partition, PartitionConfig, PartitionFamily};

use super::{ScanResult, Scanner};

const BATCH_SIZE: usize = 50;

pub struct FlowProjector {
    interval: Duration,
    partition_config: PartitionConfig,
}

impl FlowProjector {
    pub fn new(interval: Duration, partition_config: PartitionConfig) -> Self {
        Self {
            interval,
            partition_config,
        }
    }
}

impl Scanner for FlowProjector {
    fn name(&self) -> &'static str {
        "flow_projector"
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
        let tag = p.hash_tag();

        // Discover flows on this partition via SCAN
        let flow_ids = match scan_flow_ids(client, &tag).await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(partition, error = %e, "flow_projector: discovery failed");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        if flow_ids.is_empty() {
            return ScanResult { processed: 0, errors: 0 };
        }

        let now_ms = match crate::scanner::lease_expiry::server_time_ms(client).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(partition, error = %e, "flow_projector: failed to get server time");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        let mut processed: u32 = 0;
        let mut errors: u32 = 0;

        for fid_str in &flow_ids {
            match project_flow_summary(
                client, &tag, fid_str, now_ms, &self.partition_config,
            ).await {
                Ok(true) => processed += 1,
                Ok(false) => {} // no members or already up-to-date
                Err(e) => {
                    tracing::warn!(
                        partition,
                        flow_id = fid_str.as_str(),
                        error = %e,
                        "flow_projector: projection failed"
                    );
                    errors += 1;
                }
            }
        }

        ScanResult { processed, errors }
    }
}

/// Discover flow IDs on a partition via SCAN.
async fn scan_flow_ids(
    client: &ferriskey::Client,
    tag: &str,
) -> Result<Vec<String>, ferriskey::Error> {
    let mut flow_ids = Vec::new();
    let pattern = format!("ff:flow:{}:*:core", tag);
    let prefix = format!("ff:flow:{}:", tag);
    let mut cursor = "0".to_string();

    loop {
        let result: ferriskey::Value = client
            .cmd("SCAN")
            .arg(&cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg("100")
            .execute()
            .await?;

        let (next_cursor, keys) = parse_scan_result(&result);

        for key in keys {
            // Key format: ff:flow:{fp:N}:<flow_id>:core
            if let Some(rest) = key.strip_prefix(&prefix) {
                if let Some(fid) = rest.strip_suffix(":core") {
                    flow_ids.push(fid.to_string());
                }
            }
        }

        cursor = next_cursor;
        if cursor == "0" {
            break;
        }
    }

    Ok(flow_ids)
}

/// Project the summary for one flow. Returns Ok(true) if updated.
async fn project_flow_summary(
    client: &ferriskey::Client,
    tag: &str,
    fid_str: &str,
    now_ms: u64,
    config: &PartitionConfig,
) -> Result<bool, ferriskey::Error> {
    let members_key = format!("ff:flow:{}:{}:members", tag, fid_str);
    let summary_key = format!("ff:flow:{}:{}:summary", tag, fid_str);

    // Read all member execution IDs
    let member_eids: Vec<String> = client
        .cmd("SMEMBERS")
        .arg(&members_key)
        .execute()
        .await
        .unwrap_or_default();

    if member_eids.is_empty() {
        return Ok(false);
    }

    // Count public_state for each member (cross-partition reads)
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut total: u32 = 0;

    for eid_str in member_eids.iter().take(BATCH_SIZE) {
        let eid = match ff_core::types::ExecutionId::parse(eid_str) {
            Ok(id) => id,
            Err(_) => continue,
        };
        let partition = ff_core::partition::execution_partition(&eid, config);
        let ctx_tag = partition.hash_tag();
        let core_key = format!("ff:exec:{}:{}:core", ctx_tag, eid_str);

        let ps: Option<String> = client
            .cmd("HGET")
            .arg(&core_key)
            .arg("public_state")
            .execute()
            .await
            .unwrap_or(None);

        let state = ps.unwrap_or_else(|| "unknown".to_string());
        *counts.entry(state).or_insert(0) += 1;
        total += 1;
    }

    // Derive public_flow_state
    let completed = *counts.get("completed").unwrap_or(&0);
    let skipped = *counts.get("skipped").unwrap_or(&0);
    let failed = *counts.get("failed").unwrap_or(&0);
    let cancelled = *counts.get("cancelled").unwrap_or(&0);
    let expired = *counts.get("expired").unwrap_or(&0);
    let active = *counts.get("active").unwrap_or(&0);
    let suspended = *counts.get("suspended").unwrap_or(&0);
    let waiting = *counts.get("waiting").unwrap_or(&0);
    let delayed = *counts.get("delayed").unwrap_or(&0);
    let rate_limited = *counts.get("rate_limited").unwrap_or(&0);
    let waiting_children = *counts.get("waiting_children").unwrap_or(&0);

    let terminal_count = completed + skipped + failed + cancelled + expired;
    let all_terminal = terminal_count == total && total > 0;

    let flow_state = if all_terminal {
        if failed > 0 || cancelled > 0 || expired > 0 {
            "failed"
        } else {
            "completed"
        }
    } else if active > 0 {
        "running"
    } else if suspended > 0 || delayed > 0 || rate_limited > 0 || waiting_children > 0 {
        "blocked"
    } else {
        "open"
    };

    // Update summary hash
    let _: () = client
        .cmd("HSET")
        .arg(&summary_key)
        .arg("total_members").arg(total.to_string().as_str())
        .arg("members_completed").arg(completed.to_string().as_str())
        .arg("members_failed").arg(failed.to_string().as_str())
        .arg("members_cancelled").arg(cancelled.to_string().as_str())
        .arg("members_expired").arg(expired.to_string().as_str())
        .arg("members_skipped").arg(skipped.to_string().as_str())
        .arg("members_active").arg(active.to_string().as_str())
        .arg("members_suspended").arg(suspended.to_string().as_str())
        .arg("members_waiting").arg(waiting.to_string().as_str())
        .arg("members_delayed").arg(delayed.to_string().as_str())
        .arg("members_rate_limited").arg(rate_limited.to_string().as_str())
        .arg("members_waiting_children").arg(waiting_children.to_string().as_str())
        .arg("public_flow_state").arg(flow_state)
        .arg("last_summary_update_at").arg(now_ms.to_string().as_str())
        .execute()
        .await?;

    Ok(true)
}

fn parse_scan_result(value: &ferriskey::Value) -> (String, Vec<String>) {
    match value {
        ferriskey::Value::Array(arr) if arr.len() == 2 => {
            let cursor = match &arr[0] {
                Ok(ferriskey::Value::BulkString(b)) => {
                    String::from_utf8_lossy(b).to_string()
                }
                Ok(ferriskey::Value::SimpleString(s)) => s.clone(),
                _ => "0".to_string(),
            };
            let keys = match &arr[1] {
                Ok(ferriskey::Value::Array(keys)) => keys
                    .iter()
                    .filter_map(|v| match v {
                        Ok(ferriskey::Value::BulkString(b)) => {
                            Some(String::from_utf8_lossy(b).to_string())
                        }
                        Ok(ferriskey::Value::SimpleString(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => vec![],
            };
            (cursor, keys)
        }
        _ => ("0".to_string(), vec![]),
    }
}
