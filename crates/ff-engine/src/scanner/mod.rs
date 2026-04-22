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

use ff_core::backend::ScannerFilter;
use ff_core::partition::{Partition, PartitionFamily};
use ff_core::types::Namespace;
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

    /// Per-consumer filter applied by execution-shaped scanners to
    /// restrict the set of candidates they act on (issue #122).
    ///
    /// Default returns [`ScannerFilter::NOOP`] — pre-#122 behaviour.
    /// Implementations override by storing a `ScannerFilter` on the
    /// struct (constructed via `Self::with_filter(..)`) and
    /// returning `&self.filter`.
    fn filter(&self) -> &ScannerFilter {
        &ScannerFilter::NOOP
    }

    /// Scan a single partition. Called once per partition per cycle.
    fn scan_partition(
        &self,
        client: &ferriskey::Client,
        partition: u16,
    ) -> impl std::future::Future<Output = ScanResult> + Send;

    /// PR-94: per-cycle gauge sample. Returns `Some(depth)` summed
    /// across all partitions by the scanner runner to produce a
    /// single gauge value (today only `cancel_reconciler` exports
    /// one, feeding `ff_cancel_backlog_depth`). Default: `None` so
    /// scanners that don't export a gauge compile unchanged.
    ///
    /// Runs AFTER `scan_partition` for the same `partition` within
    /// the same cycle, so implementations can reuse cached state.
    /// The trivial default implementation returns `None` for every
    /// partition and the runner writes nothing.
    fn sample_backlog_depth(
        &self,
        _client: &ferriskey::Client,
        _partition: u16,
    ) -> impl std::future::Future<Output = Option<u64>> + Send {
        async { None }
    }
}

/// Issue #122: per-candidate filter helper shared by all
/// execution-shaped scanners.
///
/// Returns true iff the candidate `eid` on `partition` should be
/// SKIPPED by the scanner (i.e. the filter rejects it). A no-op
/// filter never rejects — returns false without issuing any HGET.
///
/// Cost: 0 HGET if `filter.is_noop()`; 1 HGET when only `namespace`
/// is set (on `ff:exec:{p}:<eid>:core`); 1 HGET when only
/// `instance_tag` is set (on `ff:exec:{p}:<eid>:tags`); 2 HGETs when
/// both are set. Namespace is checked first (cheaper — touches the
/// already-hot `core` hash) and short-circuits on mismatch.
///
/// On HGET failure the helper returns true (skip), conservatively:
/// leaking a cross-tenant candidate due to a transient read error is
/// worse than the scanner temporarily underclaiming — the next cycle
/// picks it back up once the backend recovers.
pub async fn should_skip_candidate(
    client: &ferriskey::Client,
    filter: &ScannerFilter,
    partition: u16,
    eid: &str,
) -> bool {
    if filter.is_noop() {
        return false;
    }
    let p = Partition {
        family: PartitionFamily::Execution,
        index: partition,
    };
    let tag = p.hash_tag();

    if let Some(ref want_ns) = filter.namespace {
        let core_key = format!("ff:exec:{}:{}:core", tag, eid);
        match client
            .cmd("HGET")
            .arg(&core_key)
            .arg("namespace")
            .execute::<Option<String>>()
            .await
        {
            Ok(Some(s)) => {
                if &Namespace::new(s) != want_ns {
                    return true;
                }
            }
            // nil or transport error — skip conservatively.
            _ => return true,
        }
    }

    if let Some((ref tag_key, ref want_value)) = filter.instance_tag {
        let tags_key = format!("ff:exec:{}:{}:tags", tag, eid);
        match client
            .cmd("HGET")
            .arg(&tags_key)
            .arg(tag_key.as_str())
            .execute::<Option<String>>()
            .await
        {
            Ok(Some(v)) if &v == want_value => {}
            _ => return true,
        }
    }

    false
}

/// Drives a scanner across all execution partitions in a loop.
pub struct ScannerRunner;

impl ScannerRunner {
    /// Spawn a tokio task that runs the scanner forever until shutdown.
    ///
    /// PR-94: the `metrics` handle records per-cycle duration +
    /// cycle-total counter. Under the no-op shim (`observability`
    /// feature off) the recorder calls compile to nothing.
    pub fn spawn<S: Scanner>(
        scanner: Arc<S>,
        client: ferriskey::Client,
        num_partitions: u16,
        mut shutdown: watch::Receiver<bool>,
        metrics: Arc<ff_observability::Metrics>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let name = scanner.name();
            let interval = scanner.interval().max(Duration::from_millis(100));
            tracing::info!(scanner = name, ?interval, partitions = num_partitions, "scanner started");

            loop {
                let cycle_start = tokio::time::Instant::now();
                let mut total_processed: u32 = 0;
                let mut total_errors: u32 = 0;
                // Gauge aggregation strategy: require **every**
                // partition to return a sample before writing.
                // A partial sum (some partitions sampled, some
                // returned None on error) would under-report and
                // make the gauge jump below the true backlog, which
                // an operator could mis-interpret as drain progress.
                // On any missing sample we set `sample_valid = false`
                // and skip the gauge write — the previous value
                // stands until a full cycle succeeds.
                //
                // First partition returning `Some(_)` sets
                // `sampled = true`; a subsequent `None` on any
                // partition invalidates the cycle's write.
                let mut total_backlog_depth: u64 = 0;
                let mut sampled = false;
                let mut sample_valid = true;

                for p in 0..num_partitions {
                    // Check for shutdown between partitions
                    if *shutdown.borrow() {
                        tracing::info!(scanner = name, "shutdown requested, stopping");
                        return;
                    }

                    let result = scanner.scan_partition(&client, p).await;
                    total_processed += result.processed;
                    total_errors += result.errors;

                    // Only query the gauge-sample hook on scanners
                    // that override it (default returns None for
                    // every partition — the check short-circuits).
                    match scanner.sample_backlog_depth(&client, p).await {
                        Some(d) => {
                            sampled = true;
                            total_backlog_depth =
                                total_backlog_depth.saturating_add(d);
                        }
                        None => {
                            // A non-overriding scanner returns None
                            // on every partition → `sampled` stays
                            // false → gauge write skipped anyway.
                            // An overriding scanner with a transient
                            // failure invalidates the cycle only if
                            // it had already sampled a partition.
                            if sampled {
                                sample_valid = false;
                            }
                        }
                    }
                }

                let elapsed = cycle_start.elapsed();
                // PR-94: scanner cycle metrics. Recorded every cycle,
                // regardless of whether the cycle did any work, so
                // operators can see cadence drift (a stuck scanner
                // stops producing data points).
                metrics.record_scanner_cycle(name, elapsed);
                if sampled && sample_valid {
                    metrics.set_cancel_backlog_depth(total_backlog_depth);
                } else if sampled && !sample_valid {
                    // At least one partition sampled but another
                    // failed — leave the gauge at its prior value.
                    // Log at debug so operators investigating a
                    // flat gauge can correlate to the cycle where
                    // sampling partially failed.
                    tracing::debug!(
                        scanner = name,
                        "skipping cancel_backlog_depth gauge write this cycle \
                         (partial partition sample)"
                    );
                }
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
