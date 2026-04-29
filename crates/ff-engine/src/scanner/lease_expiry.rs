//! Lease expiry scanner.
//!
//! Iterates `ff:idx:{p:N}:lease_expiry` for each partition, finding leases
//! whose `expires_at` score is <= now. For each, calls
//! `FCALL ff_mark_lease_expired_if_due` which re-validates and atomically
//! marks the execution as `lease_expired_reclaimable`.
//!
//! Reference: RFC-003 §Reclaim scan pattern, RFC-010 §6.1

use std::sync::Arc;
use std::time::Duration;

use ff_core::backend::ScannerFilter;
use ff_core::engine_backend::EngineBackend;
use ff_core::keys::IndexKeys;
use ff_core::partition::{Partition, PartitionFamily};
use ff_core::types::ExecutionId;

use super::{should_skip_candidate, FailureTracker, ScanResult, Scanner};

/// Batch size per ZRANGEBYSCORE call.
const BATCH_SIZE: u32 = 50;

// ─── Postgres branch (wave 6c) ──────────────────────────────────────────
#[cfg(feature = "postgres")]
pub async fn scan_tick_pg(
    pool: &ff_backend_postgres::PgPool,
    partition_key: i16,
    filter: &ff_core::backend::ScannerFilter,
) -> Result<ff_backend_postgres::reconcilers::ScanReport, ff_core::engine_error::EngineError> {
    ff_backend_postgres::reconcilers::lease_expiry::scan_tick(pool, partition_key, filter).await
}

pub struct LeaseExpiryScanner {
    interval: Duration,
    failures: FailureTracker,
    filter: ScannerFilter,
    /// PR-7b Cluster 1 plumbing — the trait-dispatch target for the
    /// per-candidate FCALL equivalent. `Engine::start_internal`
    /// constructs via [`Self::with_filter_and_backend`]; external
    /// callers (tests) that don't need filter enforcement can still
    /// use [`Self::new`] / [`Self::with_filter`], which leave the
    /// backend at `None`. When `None` + a non-noop filter is set,
    /// [`should_skip_candidate`] returns conservative-skip (same
    /// posture as a transport error).
    backend: Option<Arc<dyn EngineBackend>>,
}

impl LeaseExpiryScanner {
    pub fn new(interval: Duration) -> Self {
        Self::with_filter(interval, ScannerFilter::default())
    }

    /// Construct with a [`ScannerFilter`] applied per candidate
    /// (issue #122). See [`ScannerFilter`] rustdoc for cost.
    pub fn with_filter(interval: Duration, filter: ScannerFilter) -> Self {
        Self {
            interval,
            failures: FailureTracker::new(),
            filter,
            backend: None,
        }
    }

    /// PR-7b Cluster 1: like [`Self::with_filter`] but wires the
    /// `EngineBackend` through for trait-routed FCALLs +
    /// filter-resolution reads.
    pub fn with_filter_and_backend(
        interval: Duration,
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

impl Scanner for LeaseExpiryScanner {
    fn name(&self) -> &'static str {
        "lease_expiry"
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

        if partition == 0 {
            self.failures.advance_cycle();
        }

        if expired.is_empty() {
            return ScanResult { processed: 0, errors: 0 };
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
            if should_skip_candidate(self.backend.as_ref(), &self.filter, partition, eid_str)
                .await
            {
                continue;
            }

            let res = if let Some(ref backend) = self.backend {
                // PR-7b Cluster 1: trait-routed FCALL. The Valkey
                // impl wraps `ff_mark_lease_expired_if_due` with
                // identical KEYS + ARGV; Postgres runs a single-row
                // tx via `reconcilers::lease_expiry::release_for_execution`.
                let Ok(eid) = ExecutionId::parse(eid_str) else { tracing::warn!(execution_id=%eid_str, "malformed eid; skipping"); continue; };
                backend
                    .mark_lease_expired_if_due(p, &eid)
                    .await
                    .map_err(|e| e.to_string())
            } else {
                // Test-only fallback: direct FCALL on the supplied
                // client. Preserves the pre-trait-routing path for
                // unit tests that construct the scanner without a
                // backend.
                let exec_core = format!("ff:exec:{}:{}:core", p.hash_tag(), eid_str);
                let lease_current =
                    format!("ff:exec:{}:{}:lease:current", p.hash_tag(), eid_str);
                let lease_history =
                    format!("ff:exec:{}:{}:lease:history", p.hash_tag(), eid_str);
                let keys: [&str; 4] = [
                    &exec_core,
                    &lease_current,
                    &lease_expiry_key,
                    &lease_history,
                ];
                client
                    .fcall::<ferriskey::Value>(
                        "ff_mark_lease_expired_if_due",
                        &keys,
                        &[eid_str.as_str()],
                    )
                    .await
                    .map(|_| ())
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
                        "lease_expiry: mark_lease_expired_if_due failed"
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
