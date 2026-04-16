//! Flow summary projector scanner.
//!
//! Scans flow partitions ({fp:N}). For each flow: samples up to BATCH_SIZE
//! member executions (SRANDMEMBER), reads each member's public_state
//! (cross-partition), and updates the flow summary hash with aggregate
//! counts and derived public_flow_state.
//!
//! Interval: 15s (catchup mode — event-driven in production).
//!
//! Cluster-safe: uses SMEMBERS on a partition-level index SET instead of SCAN.
//!
//! Reference: RFC-007 §Flow Summary Projection, RFC-010 §6.7

use std::collections::HashMap;
use std::time::Duration;

use ff_core::keys::FlowIndexKeys;
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
        let fidx = FlowIndexKeys::new(&p);

        // Discover flows via partition-level index SET (cluster-safe)
        let flow_index_key = fidx.flow_index();
        let flow_ids: Vec<String> = match client
            .cmd("SMEMBERS")
            .arg(&flow_index_key)
            .execute()
            .await
        {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(partition, error = %e, "flow_projector: SMEMBERS failed");
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
                client, &tag, &flow_index_key, fid_str, now_ms, &self.partition_config,
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

/// Project the summary for one flow. Returns Ok(true) if updated.
async fn project_flow_summary(
    client: &ferriskey::Client,
    tag: &str,
    flow_index_key: &str,
    fid_str: &str,
    now_ms: u64,
    config: &PartitionConfig,
) -> Result<bool, ferriskey::Error> {
    let core_key = format!("ff:flow:{}:{}:core", tag, fid_str);
    let members_key = format!("ff:flow:{}:{}:members", tag, fid_str);
    let summary_key = format!("ff:flow:{}:{}:summary", tag, fid_str);

    // Defensive prune: index entry for a flow whose core is gone (manual
    // delete / retention purge) — drop it so SMEMBERS stays correct.
    let core_exists: bool = client.exists(&core_key).await.unwrap_or(true);
    if !core_exists {
        let _: Option<i64> = client
            .cmd("SREM")
            .arg(flow_index_key)
            .arg(fid_str)
            .execute()
            .await
            .unwrap_or(None);
        return Ok(false);
    }

    // Get true membership count for accurate total_members reporting.
    let true_total: u64 = client
        .cmd("SCARD")
        .arg(&members_key)
        .execute()
        .await
        .unwrap_or(0);

    if true_total == 0 {
        return Ok(false);
    }

    // Sample up to BATCH_SIZE member execution IDs (avoids loading the
    // entire membership set into memory for large flows).
    // SRANDMEMBER key count returns up to `count` distinct members.
    let member_eids: Vec<String> = client
        .cmd("SRANDMEMBER")
        .arg(&members_key)
        .arg(BATCH_SIZE.to_string().as_str())
        .execute()
        .await
        .unwrap_or_default();

    if member_eids.is_empty() {
        return Ok(false);
    }

    // Count public_state for each sampled member (cross-partition reads)
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut sampled: u32 = 0;

    for eid_str in &member_eids {
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
        sampled += 1;
    }

    // Derive public_flow_state from sample
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
    let all_terminal = terminal_count == sampled && sampled > 0;

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
        .arg("total_members").arg(true_total.to_string().as_str())
        .arg("sampled_members").arg(sampled.to_string().as_str())
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

    // Prune the index entry once every member is terminal. The flow core
    // lives on for replay/inspection, but keeping terminal flows in the
    // active flow_index would grow cardinality unboundedly across the
    // lifetime of the partition.
    if all_terminal {
        let _: Option<i64> = client
            .cmd("SREM")
            .arg(flow_index_key)
            .arg(fid_str)
            .execute()
            .await
            .unwrap_or(None);
    }

    Ok(true)
}

