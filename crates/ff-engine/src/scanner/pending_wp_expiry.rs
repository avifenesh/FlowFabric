//! Pending waitpoint expiry scanner.
//!
//! Iterates `ff:idx:{p:N}:pending_waitpoint_expiry` for each partition,
//! finding pending waitpoints whose `expires_at` score is <= now. For each,
//! calls `FCALL ff_close_waitpoint` to close the expired pending waitpoint.
//!
//! Reference: RFC-004 §Pending waitpoint expiry scanner, RFC-010 §6.3

use std::time::Duration;

use ff_core::backend::ScannerFilter;
use ff_core::keys::IndexKeys;
use ff_core::partition::{Partition, PartitionFamily};

use super::{should_skip_candidate, FailureTracker, ScanResult, Scanner};

const BATCH_SIZE: u32 = 100;

pub struct PendingWaitpointExpiryScanner {
    interval: Duration,
    failures: FailureTracker,
    filter: ScannerFilter,
}

impl PendingWaitpointExpiryScanner {
    pub fn new(interval: Duration) -> Self {
        Self::with_filter(interval, ScannerFilter::default())
    }

    /// Construct with a [`ScannerFilter`] applied per candidate
    /// (issue #122). The filter is resolved to the waitpoint's
    /// owning execution via the waitpoint hash's `execution_id`
    /// field, then applied via [`should_skip_candidate`].
    pub fn with_filter(interval: Duration, filter: ScannerFilter) -> Self {
        Self {
            interval,
            failures: FailureTracker::new(),
            filter,
        }
    }
}

impl Scanner for PendingWaitpointExpiryScanner {
    fn name(&self) -> &'static str {
        "pending_wp_expiry"
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
        let expiry_key = idx.pending_waitpoint_expiry();
        let tag = p.hash_tag();

        let now_ms = match crate::scanner::lease_expiry::server_time_ms(client).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(partition, error = %e, "pending_wp_expiry: failed to get server time");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        // ZRANGEBYSCORE pending_waitpoint_expiry -inf now LIMIT 0 batch_size
        let expired: Vec<String> = match client
            .cmd("ZRANGEBYSCORE")
            .arg(&expiry_key)
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
                tracing::warn!(partition, error = %e, "pending_wp_expiry: ZRANGEBYSCORE failed");
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

        for wp_id_str in &expired {
            if self.failures.should_skip(wp_id_str) {
                continue;
            }
            // Issue #122: this scanner iterates waitpoint IDs, not
            // exec IDs. To apply the filter we resolve the owning
            // execution via the waitpoint hash's `execution_id`
            // field (one HGET), then call the shared helper. When
            // the filter is a no-op this entire block is skipped.
            if !self.filter.is_noop() {
                let waitpoint_hash = format!("ff:wp:{}:{}", tag, wp_id_str);
                let eid: Option<String> = match client
                    .cmd("HGET")
                    .arg(&waitpoint_hash)
                    .arg("execution_id")
                    .execute()
                    .await
                {
                    Ok(v) => v,
                    Err(_) => {
                        // Conservative skip on transport error.
                        continue;
                    }
                };
                let Some(eid) = eid else { continue };
                if should_skip_candidate(client, &self.filter, partition, &eid).await {
                    continue;
                }
            }

            match close_expired_waitpoint(client, &tag, &idx, wp_id_str).await {
                Ok(()) => {
                    self.failures.record_success(wp_id_str);
                    processed += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        partition,
                        waitpoint_id = wp_id_str.as_str(),
                        error = %e,
                        "pending_wp_expiry: ff_close_waitpoint failed"
                    );
                    self.failures.record_failure(wp_id_str, "pending_wp_expiry");
                    errors += 1;
                }
            }
        }

        ScanResult { processed, errors }
    }
}

/// Call ff_close_waitpoint for one expired pending waitpoint.
///
/// KEYS (3): exec_core, waitpoint_hash, pending_wp_expiry_zset
/// ARGV (2): waitpoint_id, reason
///
/// NOTE: exec_core is required by the function signature but the waitpoint
/// record contains the execution_id. We read it from the waitpoint to
/// construct the exec_core key. If the waitpoint doesn't exist, the Lua
/// function handles it gracefully.
async fn close_expired_waitpoint(
    client: &ferriskey::Client,
    tag: &str,
    idx: &IndexKeys,
    wp_id_str: &str,
) -> Result<(), ferriskey::Error> {
    let waitpoint_hash = format!("ff:wp:{}:{}", tag, wp_id_str);

    // Read execution_id from waitpoint to construct exec_core key
    let eid: Option<String> = client
        .cmd("HGET")
        .arg(&waitpoint_hash)
        .arg("execution_id")
        .execute()
        .await?;

    let eid = eid.unwrap_or_default();
    let exec_core = format!("ff:exec:{}:{}:core", tag, eid);
    let pending_wp_expiry = idx.pending_waitpoint_expiry();

    let keys: [&str; 3] = [
        &exec_core,
        &waitpoint_hash,
        &pending_wp_expiry,
    ];

    let argv: [&str; 2] = [wp_id_str, "never_committed"];

    let _: ferriskey::Value = client
        .fcall("ff_close_waitpoint", &keys, &argv)
        .await?;

    Ok(())
}
