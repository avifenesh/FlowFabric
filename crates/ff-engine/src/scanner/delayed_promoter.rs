//! Delayed execution promoter scanner.
//!
//! Iterates `ff:idx:{p:N}:lane:<lane_id>:delayed` for each partition,
//! finding executions whose `delay_until` score is <= now. For each,
//! calls `FCALL ff_promote_delayed` to move them from delayed to eligible.
//!
//! Note: ff_promote_delayed is Phase 2 Lua. For Phase 1, this scanner
//! discovers due executions and logs them. The actual promotion requires
//! knowing the lane_id (embedded in the delayed ZSET key), which in turn
//! requires knowing which lanes exist. Phase 1 uses a single "default" lane.
//!
//! Reference: RFC-010 §6, function #27

use std::sync::Arc;
use std::time::Duration;

use ff_core::backend::ScannerFilter;
use ff_core::engine_backend::EngineBackend;
use ff_core::keys::IndexKeys;
use ff_core::partition::{Partition, PartitionFamily};
use ff_core::types::{ExecutionId, LaneId, TimestampMs};

use super::{should_skip_candidate, FailureTracker, ScanResult, Scanner};

const BATCH_SIZE: u32 = 50;

pub struct DelayedPromoter {
    interval: Duration,
    /// Lanes to scan. Phase 1: just "default".
    lanes: Vec<LaneId>,
    failures: FailureTracker,
    filter: ScannerFilter,
    /// PR-7b Cluster 1: trait-dispatch target. See
    /// [`super::lease_expiry::LeaseExpiryScanner::backend`] rustdoc.
    backend: Option<Arc<dyn EngineBackend>>,
}

impl DelayedPromoter {
    pub fn new(interval: Duration, lanes: Vec<LaneId>) -> Self {
        Self::with_filter(interval, lanes, ScannerFilter::default())
    }

    /// Construct with a [`ScannerFilter`] applied per candidate
    /// (issue #122).
    pub fn with_filter(interval: Duration, lanes: Vec<LaneId>, filter: ScannerFilter) -> Self {
        Self {
            interval,
            lanes,
            failures: FailureTracker::new(),
            filter,
            backend: None,
        }
    }

    /// PR-7b Cluster 1: wire an `EngineBackend` for trait-routed FCALLs.
    pub fn with_filter_and_backend(
        interval: Duration,
        lanes: Vec<LaneId>,
        filter: ScannerFilter,
        backend: Arc<dyn EngineBackend>,
    ) -> Self {
        Self {
            interval,
            lanes,
            failures: FailureTracker::new(),
            filter,
            backend: Some(backend),
        }
    }
}

impl Scanner for DelayedPromoter {
    fn name(&self) -> &'static str {
        "delayed_promoter"
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

        let now_ms = match crate::scanner::lease_expiry::server_time_ms(client).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(partition, error = %e, "delayed_promoter: failed to get server time");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        if partition == 0 {
            self.failures.advance_cycle();
        }

        let mut total_processed: u32 = 0;
        let mut total_errors: u32 = 0;

        for lane in &self.lanes {
            let delayed_key = idx.lane_delayed(lane);
            let eligible_key = idx.lane_eligible(lane);

            // ZRANGEBYSCORE delayed -inf now LIMIT 0 batch_size
            let due: Vec<String> = match client
                .cmd("ZRANGEBYSCORE")
                .arg(&delayed_key)
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
                    tracing::warn!(
                        partition, lane = %lane, error = %e,
                        "delayed_promoter: ZRANGEBYSCORE failed"
                    );
                    total_errors += 1;
                    continue;
                }
            };

            if due.is_empty() {
                continue;
            }

            // For each due execution, call ff_promote_delayed
            // KEYS(3): exec_core, delayed_zset, eligible_zset
            // ARGV(2): execution_id, now_ms
            for eid_str in &due {
                if self.failures.should_skip(eid_str) {
                    continue;
                }
                if should_skip_candidate(self.backend.as_ref(), &self.filter, partition, eid_str).await {
                    continue;
                }

                let res = if let Some(ref backend) = self.backend {
                    let Ok(eid) = ExecutionId::parse(eid_str) else { tracing::warn!(execution_id=%eid_str, "malformed eid; skipping"); continue; };
                    backend
                        .promote_delayed(p, lane, &eid, TimestampMs(now_ms as i64))
                        .await
                        .map_err(|e| e.to_string())
                } else {
                    let exec_core = format!("ff:exec:{}:{}:core", p.hash_tag(), eid_str);
                    let keys: [&str; 3] = [&exec_core, &delayed_key, &eligible_key];
                    let now_str = now_ms.to_string();
                    let argv: [&str; 2] = [eid_str.as_str(), &now_str];
                    client
                        .fcall::<ferriskey::Value>("ff_promote_delayed", &keys, &argv)
                        .await
                        .map(|_: ferriskey::Value| ())
                        .map_err(|e| e.to_string())
                };
                match res {
                    Ok(()) => {
                        self.failures.record_success(eid_str);
                        total_processed += 1;
                    }
                    Err(e) => {
                        tracing::warn!(
                            partition,
                            execution_id = eid_str.as_str(),
                            lane = %lane,
                            error = %e,
                            "delayed_promoter: promote_delayed failed"
                        );
                        self.failures.record_failure(eid_str, "delayed_promoter");
                        total_errors += 1;
                    }
                }
            }
        }

        ScanResult {
            processed: total_processed,
            errors: total_errors,
        }
    }
}

