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

use std::time::Duration;

use ff_core::keys::IndexKeys;
use ff_core::partition::{Partition, PartitionFamily};
use ff_core::types::LaneId;

use super::{ScanResult, Scanner};

const BATCH_SIZE: u32 = 50;

pub struct ExecutionDeadlineScanner {
    interval: Duration,
    lanes: Vec<LaneId>,
}

impl ExecutionDeadlineScanner {
    pub fn new(interval: Duration, lanes: Vec<LaneId>) -> Self {
        Self { interval, lanes }
    }
}

impl Scanner for ExecutionDeadlineScanner {
    fn name(&self) -> &'static str {
        "execution_deadline"
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

        if expired.is_empty() {
            return ScanResult { processed: 0, errors: 0 };
        }

        let mut processed: u32 = 0;
        let mut errors: u32 = 0;

        let lane = self.lanes.first().cloned().unwrap_or_else(|| LaneId::new("default"));

        for eid_str in &expired {
            // Reuse the same expire_execution helper as attempt_timeout —
            // ff_expire_execution handles all lifecycle phases and ZREMs from
            // both attempt_timeout and execution_deadline indexes.
            match crate::scanner::attempt_timeout::expire_execution_raw(
                client, &p, &idx, &lane, eid_str, "execution_deadline",
            ).await {
                Ok(()) => processed += 1,
                Err(e) => {
                    tracing::warn!(
                        partition,
                        execution_id = eid_str.as_str(),
                        error = %e,
                        "execution_deadline: ff_expire_execution failed"
                    );
                    errors += 1;
                }
            }
        }

        ScanResult { processed, errors }
    }
}
