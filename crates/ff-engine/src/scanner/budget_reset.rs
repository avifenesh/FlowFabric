//! Budget reset scanner.
//!
//! Scans `ff:idx:{b:M}:budget_resets` per budget partition, finding budgets
//! whose `next_reset_at` score is <= now. For each, calls
//! `FCALL ff_reset_budget` which zeroes usage counters, records the reset
//! event, computes the next reset time, and re-scores in the index.
//!
//! Reference: RFC-008 §Budget Reset, RFC-010 §6.11

use std::sync::Arc;
use std::time::Duration;

use ff_core::backend::ScannerFilter;
use ff_core::contracts::ResetBudgetArgs;
use ff_core::engine_backend::EngineBackend;
use ff_core::keys::budget_resets_key;
use ff_core::partition::{Partition, PartitionFamily};
use ff_core::types::{BudgetId, TimestampMs};

use super::{ScanResult, Scanner};

const BATCH_SIZE: u32 = 20;

pub struct BudgetResetScanner {
    interval: Duration,
    /// Issue #122: accepted for uniform API; not applied.
    filter: ScannerFilter,
    /// PR-7b Cluster 2: trait-dispatch target for the per-budget
    /// `ff_reset_budget` FCALL. When `None` (legacy test construction)
    /// the scanner falls back to the pre-trait direct-FCALL path on
    /// the supplied `ferriskey::Client`.
    backend: Option<Arc<dyn EngineBackend>>,
}

impl BudgetResetScanner {
    pub fn new(interval: Duration) -> Self {
        Self::with_filter(interval, ScannerFilter::default())
    }

    /// Accepts a [`ScannerFilter`] for uniform construction across
    /// all scanners (issue #122) but **does not apply it**. This
    /// scanner iterates budget IDs — not executions — and the
    /// `namespace` / `instance_tag` filter dimensions do not map
    /// onto budget partitions.
    pub fn with_filter(interval: Duration, filter: ScannerFilter) -> Self {
        Self {
            interval,
            filter,
            backend: None,
        }
    }

    /// PR-7b Cluster 2: wire an `EngineBackend` so the per-budget
    /// reset routes through the trait (`EngineBackend::reset_budget`)
    /// rather than issuing a raw FCALL against the scanner client.
    pub fn with_filter_and_backend(
        interval: Duration,
        filter: ScannerFilter,
        backend: Arc<dyn EngineBackend>,
    ) -> Self {
        Self {
            interval,
            filter,
            backend: Some(backend),
        }
    }
}

impl Scanner for BudgetResetScanner {
    fn name(&self) -> &'static str {
        "budget_reset"
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
            family: PartitionFamily::Budget,
            index: partition,
        };
        let tag = p.hash_tag();
        let resets_key = budget_resets_key(&tag);

        let now_ms_res: Result<u64, String> = if let Some(ref b) = self.backend {
            b.server_time_ms().await.map_err(|e| e.to_string())
        } else {
            crate::scanner::lease_expiry::server_time_ms_legacy(client).await.map_err(|e| e.to_string())
        };
        let now_ms = match now_ms_res {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(partition, error = %e, "budget_reset: failed to get server time");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        // ZRANGEBYSCORE budget_resets -inf now LIMIT 0 batch_size
        let due: Vec<String> = match client
            .cmd("ZRANGEBYSCORE")
            .arg(&resets_key)
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
                tracing::warn!(partition, error = %e, "budget_reset: ZRANGEBYSCORE failed");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        if due.is_empty() {
            return ScanResult { processed: 0, errors: 0 };
        }

        let mut processed: u32 = 0;
        let mut errors: u32 = 0;

        for budget_id_str in &due {
            let res = if let Some(ref backend) = self.backend {
                // PR-7b Cluster 2: trait-routed reset. Valkey wraps
                // `ff_reset_budget` (same KEYS/ARGV as the legacy path);
                // Postgres + SQLite run their per-row reset tx mirroring
                // the Lua semantic (`budget::reset_budget_impl`).
                let Ok(bid) = BudgetId::parse(budget_id_str) else {
                    tracing::warn!(
                        partition,
                        budget_id = budget_id_str.as_str(),
                        "budget_reset: malformed budget id; skipping"
                    );
                    errors += 1;
                    continue;
                };
                backend
                    .reset_budget(ResetBudgetArgs {
                        budget_id: bid,
                        now: TimestampMs::from_millis(now_ms as i64),
                    })
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            } else {
                // Test-only fallback: direct FCALL on the scanner client.
                // Mirrors the cluster-1 lease_expiry pattern — preserves
                // pre-trait-routing unit tests that construct the scanner
                // without a backend.
                //
                // KEYS (3): budget_def, budget_usage, budget_resets_zset
                // ARGV (2): budget_id, now_ms
                let budget_def = format!("ff:budget:{}:{}", tag, budget_id_str);
                let budget_usage = format!("ff:budget:{}:{}:usage", tag, budget_id_str);
                let keys: [&str; 3] = [&budget_def, &budget_usage, &resets_key];
                let now_s = now_ms.to_string();
                let argv: [&str; 2] = [budget_id_str.as_str(), &now_s];
                client
                    .fcall::<ferriskey::Value>("ff_reset_budget", &keys, &argv)
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            };

            match res {
                Ok(()) => processed += 1,
                Err(e) => {
                    tracing::warn!(
                        partition,
                        budget_id = budget_id_str.as_str(),
                        error = %e,
                        "budget_reset: reset_budget failed"
                    );
                    errors += 1;
                }
            }
        }

        ScanResult { processed, errors }
    }
}
