//! Attempt timeout scanner.
//!
//! Iterates `ff:idx:{p:N}:attempt_timeout` for each partition, finding
//! attempts whose timeout score is <= now. For each, calls
//! `FCALL ff_expire_execution` which handles all lifecycle phases
//! (active, runnable, suspended) and transitions to terminal(expired).
//!
//! Reference: RFC-010 §6, function #29c

use std::time::Duration;

use ff_core::keys::IndexKeys;
use ff_core::partition::{Partition, PartitionFamily};
use ff_core::types::LaneId;

use super::{ScanResult, Scanner};

const BATCH_SIZE: u32 = 50;

pub struct AttemptTimeoutScanner {
    interval: Duration,
    /// Lanes to construct index keys. Phase 1: just "default".
    lanes: Vec<LaneId>,
}

impl AttemptTimeoutScanner {
    pub fn new(interval: Duration, lanes: Vec<LaneId>) -> Self {
        Self { interval, lanes }
    }
}

impl Scanner for AttemptTimeoutScanner {
    fn name(&self) -> &'static str {
        "attempt_timeout"
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
        let timeout_key = idx.attempt_timeout();

        let now_ms = match crate::scanner::lease_expiry::server_time_ms(client).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(partition, error = %e, "attempt_timeout: failed to get server time");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        // ZRANGEBYSCORE attempt_timeout -inf now LIMIT 0 batch_size
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
                tracing::warn!(partition, error = %e, "attempt_timeout: ZRANGEBYSCORE failed");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        if timed_out.is_empty() {
            return ScanResult { processed: 0, errors: 0 };
        }

        let mut processed: u32 = 0;
        let mut errors: u32 = 0;

        // For each timed-out execution, call ff_expire_execution
        // ff_expire_execution takes 23 KEYS (RFC-010 #29c) — it handles
        // active/runnable/suspended paths with defensive ZREM from all indexes.
        // We need the lane to construct lane-scoped index keys.
        // Phase 1: use the first configured lane.
        let lane = self.lanes.first().cloned().unwrap_or_else(|| LaneId::new("default"));

        for eid_str in &timed_out {
            match expire_execution(client, &p, &idx, &lane, eid_str, "attempt_timeout").await {
                Ok(()) => processed += 1,
                Err(e) => {
                    tracing::warn!(
                        partition,
                        execution_id = eid_str.as_str(),
                        error = %e,
                        "attempt_timeout: ff_expire_execution failed"
                    );
                    errors += 1;
                }
            }
        }

        ScanResult { processed, errors }
    }
}

/// Call ff_expire_execution for one execution.
///
/// Lua KEYS (14): exec_core, attempt_hash, stream_meta, lease_current,
///                lease_history, lease_expiry_zset, worker_leases,
///                active_index, terminal_zset, attempt_timeout_zset,
///                execution_deadline_zset, suspended_zset,
///                suspension_timeout_zset, suspension_current
/// ARGV (2): execution_id, expire_reason
///
/// NOTE: attempt_hash and stream_meta use index=0 placeholder. The Lua
/// reads current_attempt_index from exec_core to find the actual attempt.
/// Same approach as ff_claim_execution — Lua constructs the real key
/// from the hash tag when the passed key doesn't match the actual index.
async fn expire_execution(
    client: &ferriskey::Client,
    partition: &Partition,
    idx: &IndexKeys,
    lane: &LaneId,
    eid_str: &str,
    reason: &str,
) -> Result<(), ferriskey::Error> {
    let tag = partition.hash_tag();

    // Entity-level keys
    let exec_core = format!("ff:exec:{}:{}:core", tag, eid_str);
    let attempt_hash = format!("ff:attempt:{}:{}:0", tag, eid_str);
    let stream_meta = format!("ff:stream:{}:{}:0:meta", tag, eid_str);
    let lease_current = format!("ff:exec:{}:{}:lease:current", tag, eid_str);
    let lease_history = format!("ff:exec:{}:{}:lease:history", tag, eid_str);
    let susp_current = format!("ff:exec:{}:{}:suspension:current", tag, eid_str);

    // Partition-level index keys
    let lease_expiry = idx.lease_expiry();
    let worker_leases = idx.worker_leases(&ff_core::types::WorkerInstanceId::new(""));
    let active = idx.lane_active(lane);
    let terminal = idx.lane_terminal(lane);
    let attempt_timeout = idx.attempt_timeout();
    let execution_deadline = idx.execution_deadline();
    let suspended = idx.lane_suspended(lane);
    let suspension_timeout = idx.suspension_timeout();

    // KEYS must match Lua's positional order exactly
    let keys: [&str; 14] = [
        &exec_core,           // 1  K.core_key
        &attempt_hash,        // 2  K.attempt_hash
        &stream_meta,         // 3  K.stream_meta
        &lease_current,       // 4  K.lease_current_key
        &lease_history,       // 5  K.lease_history_key
        &lease_expiry,        // 6  K.lease_expiry_key
        &worker_leases,       // 7  K.worker_leases_key
        &active,              // 8  K.active_index_key
        &terminal,            // 9  K.terminal_key
        &attempt_timeout,     // 10 K.attempt_timeout_key
        &execution_deadline,  // 11 K.execution_deadline_key
        &suspended,           // 12 K.suspended_zset
        &suspension_timeout,  // 13 K.suspension_timeout_key
        &susp_current,        // 14 K.suspension_current
    ];

    let argv: [&str; 2] = [eid_str, reason];

    let _: ferriskey::Value = client
        .fcall("ff_expire_execution", &keys, &argv)
        .await?;

    Ok(())
}
