//! Suspension timeout scanner.
//!
//! Iterates `ff:idx:{p:N}:suspension_timeout` for each partition, finding
//! suspended executions whose `timeout_at` score is <= now. For each, calls
//! `FCALL ff_expire_suspension` which re-validates and applies the configured
//! timeout behavior (fail/cancel/expire/auto_resume/escalate).
//!
//! Reference: RFC-004 §Timeout scanner, RFC-010 §6.2

use std::time::Duration;

use ff_core::keys::IndexKeys;
use ff_core::partition::{Partition, PartitionFamily};

use super::{ScanResult, Scanner};

const BATCH_SIZE: u32 = 50;

pub struct SuspensionTimeoutScanner {
    interval: Duration,
}

impl SuspensionTimeoutScanner {
    pub fn new(interval: Duration) -> Self {
        Self { interval }
    }
}

impl Scanner for SuspensionTimeoutScanner {
    fn name(&self) -> &'static str {
        "suspension_timeout"
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
        let timeout_key = idx.suspension_timeout();
        let tag = p.hash_tag();

        let now_ms = match crate::scanner::lease_expiry::server_time_ms(client).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(partition, error = %e, "suspension_timeout: failed to get server time");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        // ZRANGEBYSCORE suspension_timeout -inf now LIMIT 0 batch_size
        let timed_out: Vec<String> = match client
            .cmd("ZRANGEBYSCORE")
            .arg(&timeout_key)
            .arg("-inf")
            .arg(now_ms.to_string().as_str())
            .arg("LIMIT")
            .arg("0")
            .arg(BATCH_SIZE.to_string().as_str())
            .execute()
            .await
        {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(partition, error = %e, "suspension_timeout: ZRANGEBYSCORE failed");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        if timed_out.is_empty() {
            return ScanResult { processed: 0, errors: 0 };
        }

        let mut processed: u32 = 0;
        let mut errors: u32 = 0;

        for eid_str in &timed_out {
            match expire_suspension(client, &tag, &idx, eid_str).await {
                Ok(()) => processed += 1,
                Err(e) => {
                    tracing::warn!(
                        partition,
                        execution_id = eid_str.as_str(),
                        error = %e,
                        "suspension_timeout: ff_expire_suspension failed"
                    );
                    errors += 1;
                }
            }
        }

        ScanResult { processed, errors }
    }
}

/// Call ff_expire_suspension for one execution.
///
/// KEYS (12): exec_core, suspension_current, waitpoint_hash, wp_condition,
///            attempt_hash, stream_meta, suspension_timeout_zset,
///            suspended_zset, terminal_zset, eligible_zset, delayed_zset,
///            lease_history
/// ARGV (1): execution_id
///
/// NOTE: waitpoint_hash, wp_condition, attempt_hash, and stream_meta use
/// placeholder values (attempt_index=0, waitpoint_id=placeholder).
/// The Lua reads current_waitpoint_id and current_attempt_index from
/// exec_core/suspension_current to find the actual keys.
/// All keys share the same {p:N} hash tag → same cluster slot.
async fn expire_suspension(
    client: &ferriskey::Client,
    tag: &str,
    idx: &IndexKeys,
    eid_str: &str,
) -> Result<(), ferriskey::Error> {
    // Entity-level keys — we need exec_core and suspension_current.
    // For waitpoint/attempt, we need the real IDs. Read them from exec_core.
    let exec_core = format!("ff:exec:{}:{}:core", tag, eid_str);
    let suspension_current = format!("ff:exec:{}:{}:suspension:current", tag, eid_str);

    // Read current_waitpoint_id and current_attempt_index from exec_core
    let wp_id: Option<String> = client
        .cmd("HGET")
        .arg(&exec_core)
        .arg("current_waitpoint_id")
        .execute()
        .await?;
    let att_idx: Option<String> = client
        .cmd("HGET")
        .arg(&exec_core)
        .arg("current_attempt_index")
        .execute()
        .await?;

    let wp_id = wp_id.unwrap_or_default();
    let att_idx = att_idx.unwrap_or_else(|| "0".to_string());

    let waitpoint_hash = format!("ff:wp:{}:{}", tag, wp_id);
    let wp_condition = format!("ff:wp:{}:{}:condition", tag, wp_id);
    let attempt_hash = format!("ff:attempt:{}:{}:{}", tag, eid_str, att_idx);
    let stream_meta = format!("ff:stream:{}:{}:{}:meta", tag, eid_str, att_idx);

    // Partition-level index keys
    let suspension_timeout = idx.suspension_timeout();
    // We need lane for suspended_zset + terminal + eligible + delayed.
    // Read lane from exec_core.
    let lane: Option<String> = client
        .cmd("HGET")
        .arg(&exec_core)
        .arg("current_lane")
        .execute()
        .await?;
    let lane_str = lane.unwrap_or_else(|| "default".to_string());
    let lane_id = ff_core::types::LaneId::new(&lane_str);

    let suspended_zset = idx.lane_suspended(&lane_id);
    let terminal_zset = idx.lane_terminal(&lane_id);
    let eligible_zset = idx.lane_eligible(&lane_id);
    let delayed_zset = idx.lane_delayed(&lane_id);
    let lease_history = format!("ff:exec:{}:{}:lease:history", tag, eid_str);

    let keys: [&str; 12] = [
        &exec_core,           // 1
        &suspension_current,  // 2
        &waitpoint_hash,      // 3
        &wp_condition,        // 4
        &attempt_hash,        // 5
        &stream_meta,         // 6
        &suspension_timeout,  // 7
        &suspended_zset,      // 8
        &terminal_zset,       // 9
        &eligible_zset,       // 10
        &delayed_zset,        // 11
        &lease_history,       // 12
    ];

    let argv: [&str; 1] = [eid_str];

    let _: ferriskey::Value = client
        .fcall("ff_expire_suspension", &keys, &argv)
        .await?;

    Ok(())
}
