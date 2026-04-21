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
pub mod cancel_reconciler;
pub mod dependency_reconciler;
pub mod flow_projector;
pub mod index_reconciler;
pub mod lease_expiry;
pub mod pending_wp_expiry;
pub mod quota_reconciler;
pub mod retention_trimmer;
pub mod suspension_timeout;
pub mod unblock;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Result of scanning one partition.
pub struct ScanResult {
    pub processed: u32,
    pub errors: u32,
}

// ── Failure tracking for persistent FCALL errors ──

/// Max consecutive failures before an item enters backoff.
const FAILURE_THRESHOLD: u32 = 3;
/// Number of scan cycles to skip after hitting the threshold.
const BACKOFF_CYCLES: u64 = 10;
/// Max tracked entries before GC runs.
const GC_THRESHOLD: usize = 500;

struct FailureEntry {
    consecutive_failures: u32,
    skip_until_cycle: u64,
}

/// Tracks persistently-failing items so they don't permanently consume
/// batch slots. After [`FAILURE_THRESHOLD`] consecutive failures for the
/// same key, the item is skipped for [`BACKOFF_CYCLES`] scan cycles.
#[derive(Default)]
pub struct FailureTracker {
    inner: Mutex<HashMap<String, FailureEntry>>,
    cycle: AtomicU64,
}

impl FailureTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Call once per full scan cycle (e.g., when partition == 0).
    pub fn advance_cycle(&self) {
        let cycle = self.cycle.fetch_add(1, Ordering::Relaxed) + 1;
        // Periodic GC: remove entries whose backoff has expired
        if cycle.is_multiple_of(50) {
            let mut map = self.inner.lock().unwrap();
            if map.len() > GC_THRESHOLD {
                map.retain(|_, e| {
                    e.consecutive_failures >= FAILURE_THRESHOLD
                        && e.skip_until_cycle > cycle
                });
            }
        }
    }

    /// Returns true if this item should be skipped (in backoff).
    /// Also resets the entry when backoff expires, giving it another chance.
    pub fn should_skip(&self, key: &str) -> bool {
        let mut map = self.inner.lock().unwrap();
        if let Some(entry) = map.get_mut(key)
            && entry.consecutive_failures >= FAILURE_THRESHOLD
        {
            let cycle = self.cycle.load(Ordering::Relaxed);
            if entry.skip_until_cycle > cycle {
                return true;
            }
            // Backoff expired — reset and allow retry
            entry.consecutive_failures = 0;
            entry.skip_until_cycle = 0;
        }
        false
    }

    /// Record a failure. After [`FAILURE_THRESHOLD`] consecutive failures,
    /// logs an error and puts the item into backoff.
    pub fn record_failure(&self, key: &str, scanner_name: &str) {
        let mut map = self.inner.lock().unwrap();
        let entry = map.entry(key.to_owned()).or_insert(FailureEntry {
            consecutive_failures: 0,
            skip_until_cycle: 0,
        });
        entry.consecutive_failures += 1;
        if entry.consecutive_failures == FAILURE_THRESHOLD {
            let cycle = self.cycle.load(Ordering::Relaxed);
            entry.skip_until_cycle = cycle + BACKOFF_CYCLES;
            tracing::error!(
                scanner = scanner_name,
                item = key,
                failures = entry.consecutive_failures,
                backoff_cycles = BACKOFF_CYCLES,
                "persistent FCALL failure — skipping for {BACKOFF_CYCLES} scan cycles"
            );
        }
    }

    /// Record a success — clears any tracked failure state.
    pub fn record_success(&self, key: &str) {
        let mut map = self.inner.lock().unwrap();
        map.remove(key);
    }
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
            let interval = scanner.interval().max(Duration::from_millis(100));
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
