//! Attempt timeout scanner.
//!
//! Iterates `ff:idx:{p:N}:attempt_timeout` for each partition, finding
//! attempts whose timeout score is <= now. For each, calls
//! `FCALL ff_expire_execution` which handles all lifecycle phases
//! (active, runnable, suspended) and transitions to terminal(expired).
//!
//! Reference: RFC-010 §6, function #29c

use std::sync::Arc;
use std::time::Duration;

use ff_core::backend::ScannerFilter;
use ff_core::engine_backend::{EngineBackend, ExpirePhase};
use ff_core::keys::IndexKeys;
use ff_core::partition::{Partition, PartitionFamily};
use ff_core::types::{ExecutionId, LaneId, TimestampMs};

use super::{should_skip_candidate, FailureTracker, ScanResult, Scanner};

const BATCH_SIZE: u32 = 50;

// ─── Postgres branch (wave 6c) ──────────────────────────────────────────
//
// Parallel entry point for deployments running the Postgres backend.
// Delegates to `ff_backend_postgres::reconcilers::attempt_timeout`. The
// Valkey scan path above is untouched. The engine's driver picks this
// branch when the configured backend is Postgres; same shape as
// `dispatch_via_postgres` (partition_router.rs).
#[cfg(feature = "postgres")]
pub async fn scan_tick_pg(
    pool: &ff_backend_postgres::PgPool,
    partition_key: i16,
    filter: &ff_core::backend::ScannerFilter,
) -> Result<ff_backend_postgres::reconcilers::ScanReport, ff_core::engine_error::EngineError> {
    ff_backend_postgres::reconcilers::attempt_timeout::scan_tick(pool, partition_key, filter).await
}

pub struct AttemptTimeoutScanner {
    interval: Duration,
    failures: FailureTracker,
    filter: ScannerFilter,
    backend: Option<Arc<dyn EngineBackend>>,
}

impl AttemptTimeoutScanner {
    pub fn new(interval: Duration, lanes: Vec<LaneId>) -> Self {
        Self::with_filter(interval, lanes, ScannerFilter::default())
    }

    /// Construct with a [`ScannerFilter`] applied per candidate
    /// (issue #122).
    pub fn with_filter(
        interval: Duration,
        _lanes: Vec<LaneId>,
        filter: ScannerFilter,
    ) -> Self {
        Self {
            interval,
            failures: FailureTracker::new(),
            filter,
            backend: None,
        }
    }

    /// PR-7b Cluster 1: wire an `EngineBackend` for trait-routed FCALLs.
    pub fn with_filter_and_backend(
        interval: Duration,
        _lanes: Vec<LaneId>,
        filter: ScannerFilter,
        backend: Arc<dyn EngineBackend>,
    ) -> Self {
        Self {
            interval,
            failures: FailureTracker::new(),
            filter,
            backend: Some(backend),
        }
    }
}

impl Scanner for AttemptTimeoutScanner {
    fn name(&self) -> &'static str {
        "attempt_timeout"
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
        let timeout_key = idx.attempt_timeout();

        let now_ms_res: Result<u64, String> = if let Some(ref b) = self.backend {
            b.server_time_ms().await.map_err(|e| e.to_string())
        } else {
            crate::scanner::lease_expiry::server_time_ms_legacy(client).await.map_err(|e| e.to_string())
        };
        let now_ms = match now_ms_res {
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

        if partition == 0 {
            self.failures.advance_cycle();
        }

        if timed_out.is_empty() {
            return ScanResult { processed: 0, errors: 0 };
        }

        let mut processed: u32 = 0;
        let mut errors: u32 = 0;

        for eid_str in &timed_out {
            if self.failures.should_skip(eid_str) {
                continue;
            }
            if should_skip_candidate(self.backend.as_ref(), &self.filter, partition, eid_str).await {
                continue;
            }

            let res = if let Some(ref backend) = self.backend {
                let Ok(eid) = ExecutionId::parse(eid_str) else { tracing::warn!(execution_id=%eid_str, "malformed eid; skipping"); continue; };
                backend
                    .expire_execution(p, &eid, ExpirePhase::AttemptTimeout, TimestampMs(now_ms as i64))
                    .await
                    .map_err(|e| e.to_string())
            } else {
                expire_execution_raw(client, &p, &idx, eid_str, "attempt_timeout")
                    .await
                    .map_err(|e| e.to_string())
            };
            match res {
                Ok(()) => {
                    self.failures.record_success(eid_str);
                    processed += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        partition,
                        execution_id = eid_str.as_str(),
                        error = %e,
                        "attempt_timeout: expire_execution failed"
                    );
                    self.failures.record_failure(eid_str, "attempt_timeout");
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
/// Pre-reads `lane_id` from exec_core so that lane-scoped index keys
/// (active, terminal, suspended) point to the correct ZSET. Same pattern
/// as suspension_timeout::expire_suspension.
///
/// Public so execution_deadline scanner can reuse it.
pub async fn expire_execution_raw(
    client: &ferriskey::Client,
    partition: &Partition,
    idx: &IndexKeys,
    eid_str: &str,
    reason: &str,
) -> Result<(), ferriskey::Error> {
    let tag = partition.hash_tag();

    // Entity-level keys
    let exec_core = format!("ff:exec:{}:{}:core", tag, eid_str);
    let lease_current = format!("ff:exec:{}:{}:lease:current", tag, eid_str);
    let lease_history = format!("ff:exec:{}:{}:lease:history", tag, eid_str);
    let susp_current = format!("ff:exec:{}:{}:suspension:current", tag, eid_str);

    // Pre-read lane_id and current_attempt_index from exec_core
    // (same pattern as suspension_timeout — need real attempt index for
    // correct attempt_hash and stream_meta keys)
    let pre_fields: Vec<Option<String>> = client
        .cmd("HMGET")
        .arg(&exec_core)
        .arg("lane_id")
        .arg("current_attempt_index")
        .execute()
        .await?;
    let lane = ff_core::types::LaneId::new(
        pre_fields.first()
            .and_then(|v| v.as_deref())
            .unwrap_or("default"),
    );
    let att_idx = pre_fields.get(1)
        .and_then(|v| v.as_deref())
        .unwrap_or("0");

    let attempt_hash = format!("ff:attempt:{}:{}:{}", tag, eid_str, att_idx);
    let stream_meta = format!("ff:stream:{}:{}:{}:meta", tag, eid_str, att_idx);

    // Partition-level index keys
    let lease_expiry = idx.lease_expiry();
    let worker_leases = idx.worker_leases(&ff_core::types::WorkerInstanceId::new(""));
    let active = idx.lane_active(&lane);
    let terminal = idx.lane_terminal(&lane);
    let attempt_timeout = idx.attempt_timeout();
    let execution_deadline = idx.execution_deadline();
    let suspended = idx.lane_suspended(&lane);
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
