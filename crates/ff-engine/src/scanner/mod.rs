//! Background scanner infrastructure.
//!
//! Scanners iterate execution partitions at fixed intervals, checking for
//! conditions that require action (expired leases, due delays, index drift).
//! Each scanner type implements the `Scanner` trait; the `ScannerRunner`
//! drives them as tokio tasks.

pub mod attempt_timeout;
pub mod budget_reconciler;
pub mod execution_deadline;
pub mod budget_reset;
pub mod delayed_promoter;
pub mod dependency_reconciler;
pub mod flow_projector;
pub mod index_reconciler;
pub mod lease_expiry;
pub mod pending_wp_expiry;
pub mod quota_reconciler;
pub mod retention_trimmer;
pub mod suspension_timeout;
pub mod unblock;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Result of scanning one partition.
pub struct ScanResult {
    pub processed: u32,
    pub errors: u32,
}

/// Trait for background partition scanners.
///
/// Each implementation scans one aspect of execution state (lease expiry,
/// delayed promotion, index consistency) across all partitions at a
/// configured interval.
pub trait Scanner: Send + Sync + 'static {
    /// Human-readable name for logging.
    fn name(&self) -> &'static str;

    /// How often to run a full scan across all partitions.
    fn interval(&self) -> Duration;

    /// Scan a single partition. Called once per partition per cycle.
    fn scan_partition(
        &self,
        client: &ferriskey::Client,
        partition: u16,
    ) -> impl std::future::Future<Output = ScanResult> + Send;
}

/// Drives a scanner across all execution partitions in a loop.
pub struct ScannerRunner;

impl ScannerRunner {
    /// Spawn a tokio task that runs the scanner forever until shutdown.
    pub fn spawn<S: Scanner>(
        scanner: Arc<S>,
        client: ferriskey::Client,
        num_partitions: u16,
        mut shutdown: watch::Receiver<bool>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let name = scanner.name();
            let interval = scanner.interval();
            tracing::info!(scanner = name, ?interval, partitions = num_partitions, "scanner started");

            loop {
                let cycle_start = tokio::time::Instant::now();
                let mut total_processed: u32 = 0;
                let mut total_errors: u32 = 0;

                for p in 0..num_partitions {
                    // Check for shutdown between partitions
                    if *shutdown.borrow() {
                        tracing::info!(scanner = name, "shutdown requested, stopping");
                        return;
                    }

                    let result = scanner.scan_partition(&client, p).await;
                    total_processed += result.processed;
                    total_errors += result.errors;
                }

                let elapsed = cycle_start.elapsed();
                if total_processed > 0 || total_errors > 0 {
                    tracing::info!(
                        scanner = name,
                        processed = total_processed,
                        errors = total_errors,
                        elapsed_ms = elapsed.as_millis() as u64,
                        "scan cycle complete"
                    );
                } else {
                    tracing::trace!(
                        scanner = name,
                        elapsed_ms = elapsed.as_millis() as u64,
                        "scan cycle complete (nothing to do)"
                    );
                }

                // Sleep for the remaining interval (or immediately if scan took longer)
                let sleep_dur = interval.saturating_sub(elapsed);
                tokio::select! {
                    _ = tokio::time::sleep(sleep_dur) => {}
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            tracing::info!(scanner = name, "shutdown requested, stopping");
                            return;
                        }
                    }
                }
            }
        })
    }
}
