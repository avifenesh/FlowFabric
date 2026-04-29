//! Execution deadline scanner.
//!
//! Iterates `ff:idx:{p:N}:execution_deadline` for each partition, finding
//! executions whose absolute deadline score is <= now. For each, calls
//! `FCALL ff_expire_execution` which handles all lifecycle phases
//! (active, runnable, suspended) and transitions to terminal(expired).
//!
//! This is distinct from the attempt_timeout scanner: attempt_timeout
//! tracks per-attempt relative deadlines, while execution_deadline tracks
//! the absolute maximum lifetime of the entire execution regardless of
//! retries, suspensions, or delays.
//!
//! Reference: RFC-001 §execution_deadline, RFC-010 §6

use std::sync::Arc;
use std::time::Duration;

use ff_core::backend::ScannerFilter;
use ff_core::engine_backend::{EngineBackend, ExpirePhase};
use ff_core::keys::IndexKeys;
use ff_core::partition::{Partition, PartitionFamily};
use ff_core::types::{ExecutionId, TimestampMs};

use super::{should_skip_candidate, FailureTracker, ScanResult, Scanner};

const BATCH_SIZE: u32 = 50;

pub struct ExecutionDeadlineScanner {
    interval: Duration,
    failures: FailureTracker,
    filter: ScannerFilter,
    backend: Option<Arc<dyn EngineBackend>>,
}

impl ExecutionDeadlineScanner {
    pub fn new(interval: Duration, lanes: Vec<ff_core::types::LaneId>) -> Self {
        Self::with_filter(interval, lanes, ScannerFilter::default())
    }

    /// Construct with a [`ScannerFilter`] applied per candidate
    /// (issue #122).
    pub fn with_filter(
        interval: Duration,
        _lanes: Vec<ff_core::types::LaneId>,
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
        _lanes: Vec<ff_core::types::LaneId>,
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

impl Scanner for ExecutionDeadlineScanner {
    fn name(&self) -> &'static str {
        "execution_deadline"
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
        let deadline_key = idx.execution_deadline();

        let now_ms = match crate::scanner::lease_expiry::server_time_ms(client).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(partition, error = %e, "execution_deadline: failed to get server time");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        // ZRANGEBYSCORE execution_deadline -inf now LIMIT 0 batch_size
        let expired: Vec<String> = match client
            .cmd("ZRANGEBYSCORE")
            .arg(&deadline_key)
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
                tracing::warn!(partition, error = %e, "execution_deadline: ZRANGEBYSCORE failed");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        if partition == 0 {
            self.failures.advance_cycle();
        }

        if expired.is_empty() {
            return ScanResult { processed: 0, errors: 0 };
        }

        let mut processed: u32 = 0;
        let mut errors: u32 = 0;

        for eid_str in &expired {
            if self.failures.should_skip(eid_str) {
                continue;
            }
            if should_skip_candidate(self.backend.as_ref(), &self.filter, partition, eid_str).await {
                continue;
            }

            let res = if let Some(ref backend) = self.backend {
                let Ok(eid) = ExecutionId::parse(eid_str) else { tracing::warn!(execution_id=%eid_str, "malformed eid; skipping"); continue; };
                backend
                    .expire_execution(
                        p,
                        &eid,
                        ExpirePhase::ExecutionDeadline,
                        TimestampMs(now_ms as i64),
                    )
                    .await
                    .map_err(|e| e.to_string())
            } else {
                crate::scanner::attempt_timeout::expire_execution_raw(
                    client,
                    &p,
                    &idx,
                    eid_str,
                    "execution_deadline",
                )
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
                        "execution_deadline: expire_execution failed"
                    );
                    self.failures.record_failure(eid_str, "execution_deadline");
                    errors += 1;
                }
            }
        }

        ScanResult { processed, errors }
    }
}
