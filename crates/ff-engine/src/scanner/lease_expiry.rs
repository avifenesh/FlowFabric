//! Lease expiry scanner.
//!
//! Iterates `ff:idx:{p:N}:lease_expiry` for each partition, finding leases
//! whose `expires_at` score is <= now. For each, calls
//! `FCALL ff_mark_lease_expired_if_due` which re-validates and atomically
//! marks the execution as `lease_expired_reclaimable`.
//!
//! Reference: RFC-003 §Reclaim scan pattern, RFC-010 §6.1

use std::time::Duration;

use ff_core::keys::IndexKeys;
use ff_core::partition::{Partition, PartitionFamily};

use super::{FailureTracker, ScanResult, Scanner};

/// Batch size per ZRANGEBYSCORE call.
const BATCH_SIZE: u32 = 50;

pub struct LeaseExpiryScanner {
    interval: Duration,
    failures: FailureTracker,
}

impl LeaseExpiryScanner {
    pub fn new(interval: Duration) -> Self {
        Self { interval, failures: FailureTracker::new() }
    }
}

impl Scanner for LeaseExpiryScanner {
    fn name(&self) -> &'static str {
        "lease_expiry"
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
        let lease_expiry_key = idx.lease_expiry();

        // Get current server time
        let now_ms = match server_time_ms(client).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(partition, error = %e, "lease_expiry: failed to get server time");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        // ZRANGEBYSCORE lease_expiry -inf now LIMIT 0 batch_size
        let expired: Vec<String> = match client
            .cmd("ZRANGEBYSCORE")
            .arg(&lease_expiry_key)
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
                tracing::warn!(partition, error = %e, "lease_expiry: ZRANGEBYSCORE failed");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        if expired.is_empty() {
            return ScanResult { processed: 0, errors: 0 };
        }

        if partition == 0 {
            self.failures.advance_cycle();
        }

        let mut processed: u32 = 0;
        let mut errors: u32 = 0;

        // For each expired execution, call ff_mark_lease_expired_if_due
        // KEYS(4): exec_core, lease_current, lease_expiry_zset, lease_history
        // ARGV(1): execution_id
        for eid_str in &expired {
            if self.failures.should_skip(eid_str) {
                continue;
            }

            let exec_core = format!("ff:exec:{}:{}:core", p.hash_tag(), eid_str);
            let lease_current = format!("ff:exec:{}:{}:lease:current", p.hash_tag(), eid_str);
            let lease_history = format!("ff:exec:{}:{}:lease:history", p.hash_tag(), eid_str);

            let keys: [&str; 4] = [
                &exec_core,
                &lease_current,
                &lease_expiry_key,
                &lease_history,
            ];

            match client.fcall::<ferriskey::Value>(
                "ff_mark_lease_expired_if_due",
                &keys,
                &[eid_str.as_str()],
            ).await {
                Ok(_) => {
                    self.failures.record_success(eid_str);
                    processed += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        partition,
                        execution_id = eid_str.as_str(),
                        error = %e,
                        "lease_expiry: ff_mark_lease_expired_if_due failed"
                    );
                    self.failures.record_failure(eid_str, "lease_expiry");
                    errors += 1;
                }
            }
        }

        ScanResult { processed, errors }
    }
}

/// Get server time in milliseconds via the TIME command.
pub(crate) async fn server_time_ms(client: &ferriskey::Client) -> Result<u64, ferriskey::Error> {
    let result: Vec<String> = client
        .cmd("TIME")
        .execute()
        .await?;
    if result.len() < 2 {
        return Err(ferriskey::Error::from((
            ferriskey::ErrorKind::ClientError,
            "TIME returned fewer than 2 elements",
        )));
    }
    let secs: u64 = result[0].parse().map_err(|_| {
        ferriskey::Error::from((ferriskey::ErrorKind::ClientError, "TIME: invalid seconds"))
    })?;
    let micros: u64 = result[1].parse().map_err(|_| {
        ferriskey::Error::from((ferriskey::ErrorKind::ClientError, "TIME: invalid microseconds"))
    })?;
    Ok(secs * 1000 + micros / 1000)
}
