//! Postgres scanner supervisor (RFC-017 Wave 8 Stage E3).
//!
//! Owns a tokio [`JoinSet`] of reconciler-tick tasks — the Postgres
//! twin of `ff-engine`'s Valkey scanner supervisor. Each reconciler
//! runs on its configured interval; ticks iterate over the partition
//! space (for the partition-scoped reconcilers: `attempt_timeout`,
//! `lease_expiry`, `suspension_timeout`) or run once per tick (for
//! the global-scan reconcilers: `dependency`, `edge_cancel_dispatcher`,
//! `edge_cancel_reconciler`). A per-tick error is logged at `warn`
//! and does not abort the task — matches the Valkey scanner
//! semantics where a bad partition does not poison the scanner.
//!
//! # Shutdown
//!
//! [`PostgresScannerHandle::shutdown`] flips a `watch` channel;
//! running tasks observe it at the next tick boundary and exit.
//! The `grace` timeout bounds the wait on the underlying JoinSet.

use std::sync::Arc;
use std::time::Duration;

use ff_core::backend::ScannerFilter;
use ff_core::partition::PartitionConfig;
use sqlx::PgPool;
use tokio::sync::{watch, Mutex};
use tokio::task::JoinSet;

use crate::reconcilers;

/// Subset of `EngineConfig`'s interval knobs that the Postgres
/// reconcilers honour. Mirrors the Valkey engine's per-scanner
/// interval fields so `ServerConfig::engine_config` can thread the
/// same environment values through to both backends.
#[derive(Clone, Debug)]
pub struct PostgresScannerConfig {
    pub attempt_timeout_interval: Duration,
    pub lease_expiry_interval: Duration,
    pub suspension_timeout_interval: Duration,
    pub dependency_reconciler_interval: Duration,
    pub edge_cancel_dispatcher_interval: Duration,
    pub edge_cancel_reconciler_interval: Duration,
    /// Stale-threshold for the dependency reconciler (ms). Matches
    /// the Valkey scanner's `stale_threshold_ms` knob.
    pub dependency_stale_threshold_ms: i64,
    pub scanner_filter: ScannerFilter,
    pub partition_config: PartitionConfig,
}

impl PostgresScannerConfig {
    /// Default threshold mirrors the Valkey dep reconciler (15s — a
    /// full scan cycle).
    pub const DEFAULT_DEP_STALE_MS: i64 = 15_000;
}

/// Spawned scanner supervisor. Holding this handle keeps the scanner
/// tasks alive; dropping it leaves them running until shutdown. Call
/// [`shutdown`](Self::shutdown) with a bounded `grace` to drain.
pub struct PostgresScannerHandle {
    shutdown_tx: watch::Sender<bool>,
    join_set: Arc<Mutex<JoinSet<()>>>,
}

impl PostgresScannerHandle {
    /// Signal shutdown and await drain up to `grace`. Returns the
    /// number of tasks that did not exit cleanly within `grace` (for
    /// operator logging). Subsequent calls are no-ops.
    pub async fn shutdown(&self, grace: Duration) -> usize {
        let _ = self.shutdown_tx.send(true);
        let mut js = self.join_set.lock().await;
        let deadline = tokio::time::Instant::now() + grace;
        let mut timed_out = 0usize;
        while !js.is_empty() {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);
            if remaining.is_zero() {
                timed_out = js.len();
                js.abort_all();
                break;
            }
            match tokio::time::timeout(remaining, js.join_next()).await {
                Ok(Some(_res)) => continue,
                Ok(None) => break,
                Err(_) => {
                    timed_out = js.len();
                    js.abort_all();
                    break;
                }
            }
        }
        timed_out
    }
}

/// Spawn all six Postgres reconcilers as long-lived tick loops.
///
/// Per-task shape:
///   1. Build a `tokio::time::interval(cfg.<reconciler>_interval)`.
///   2. On each tick (or on shutdown signal), either run the
///      reconciler tick body or exit.
///   3. Log per-tick failures at `warn` and continue — matches the
///      Valkey scanner's "don't poison the scanner on one bad
///      partition" semantic.
pub fn spawn_scanners(pool: PgPool, cfg: PostgresScannerConfig) -> PostgresScannerHandle {
    let (tx, rx) = watch::channel(false);
    let js = Arc::new(Mutex::new(JoinSet::new()));

    // Capture partition count up-front so each task doesn't need the
    // full PartitionConfig reference.
    let num_partitions: i16 = cfg.partition_config.num_flow_partitions as i16;
    let filter = cfg.scanner_filter.clone();

    // ── Partition-scoped reconcilers ──
    spawn_partition_scan(
        &js,
        &tx,
        rx.clone(),
        pool.clone(),
        cfg.attempt_timeout_interval,
        num_partitions,
        filter.clone(),
        "pg.attempt_timeout",
        |pool, part, filter| Box::pin(async move {
            reconcilers::attempt_timeout::scan_tick(&pool, part, &filter)
                .await
                .map(|_| ())
        }),
    );
    spawn_partition_scan(
        &js,
        &tx,
        rx.clone(),
        pool.clone(),
        cfg.lease_expiry_interval,
        num_partitions,
        filter.clone(),
        "pg.lease_expiry",
        |pool, part, filter| Box::pin(async move {
            reconcilers::lease_expiry::scan_tick(&pool, part, &filter)
                .await
                .map(|_| ())
        }),
    );
    spawn_partition_scan(
        &js,
        &tx,
        rx.clone(),
        pool.clone(),
        cfg.suspension_timeout_interval,
        num_partitions,
        filter.clone(),
        "pg.suspension_timeout",
        |pool, part, filter| Box::pin(async move {
            reconcilers::suspension_timeout::scan_tick(&pool, part, &filter)
                .await
                .map(|_| ())
        }),
    );

    // ── Global (non-partition-scoped) reconcilers ──
    let dep_stale = cfg.dependency_stale_threshold_ms;
    spawn_global_scan(
        &js,
        &tx,
        rx.clone(),
        pool.clone(),
        cfg.dependency_reconciler_interval,
        filter.clone(),
        "pg.dependency",
        move |pool, filter| {
            Box::pin(async move {
                reconcilers::dependency::reconcile_tick(&pool, &filter, dep_stale)
                    .await
                    .map(|_| ())
            })
        },
    );
    spawn_global_scan(
        &js,
        &tx,
        rx.clone(),
        pool.clone(),
        cfg.edge_cancel_dispatcher_interval,
        filter.clone(),
        "pg.edge_cancel_dispatcher",
        |pool, filter| {
            Box::pin(async move {
                reconcilers::edge_cancel_dispatcher::dispatcher_tick(&pool, &filter)
                    .await
                    .map(|_| ())
            })
        },
    );
    spawn_global_scan(
        &js,
        &tx,
        rx,
        pool,
        cfg.edge_cancel_reconciler_interval,
        filter,
        "pg.edge_cancel_reconciler",
        |pool, filter| {
            Box::pin(async move {
                reconcilers::edge_cancel_reconciler::reconciler_tick(&pool, &filter)
                    .await
                    .map(|_| ())
            })
        },
    );

    tracing::info!(
        scanners = 6,
        num_partitions,
        "postgres scanner supervisor spawned (RFC-017 Stage E3)"
    );

    PostgresScannerHandle {
        shutdown_tx: tx,
        join_set: js,
    }
}

type TickFut = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ff_core::engine_error::EngineError>> + Send>>;

#[allow(clippy::too_many_arguments)]
fn spawn_partition_scan<F>(
    js: &Arc<Mutex<JoinSet<()>>>,
    _tx: &watch::Sender<bool>,
    mut shutdown: watch::Receiver<bool>,
    pool: PgPool,
    interval: Duration,
    num_partitions: i16,
    filter: ScannerFilter,
    name: &'static str,
    tick: F,
) where
    F: Fn(PgPool, i16, ScannerFilter) -> TickFut + Send + Sync + 'static + Clone,
{
    let js_clone = js.clone();
    tokio::spawn(async move {
        // Use try_lock / block_on-free lock via Mutex::lock — JoinSet
        // itself is !Send across tokio spawn boundaries only via its
        // internal handles, but the Arc<Mutex<>> holds it cleanly.
        let mut guard = js_clone.lock().await;
        guard.spawn(async move {
            let mut tk = tokio::time::interval(interval);
            tk.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            return;
                        }
                    }
                    _ = tk.tick() => {
                        for part in 0..num_partitions {
                            if *shutdown.borrow() {
                                return;
                            }
                            if let Err(e) = tick(pool.clone(), part, filter.clone()).await {
                                tracing::warn!(
                                    scanner = name,
                                    partition = part,
                                    error = %e,
                                    "postgres reconciler tick failed"
                                );
                            }
                        }
                    }
                }
            }
        });
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_global_scan<F>(
    js: &Arc<Mutex<JoinSet<()>>>,
    _tx: &watch::Sender<bool>,
    mut shutdown: watch::Receiver<bool>,
    pool: PgPool,
    interval: Duration,
    filter: ScannerFilter,
    name: &'static str,
    tick: F,
) where
    F: Fn(PgPool, ScannerFilter) -> TickFut + Send + Sync + 'static + Clone,
{
    let js_clone = js.clone();
    tokio::spawn(async move {
        let mut guard = js_clone.lock().await;
        guard.spawn(async move {
            let mut tk = tokio::time::interval(interval);
            tk.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            return;
                        }
                    }
                    _ = tk.tick() => {
                        if *shutdown.borrow() {
                            return;
                        }
                        if let Err(e) = tick(pool.clone(), filter.clone()).await {
                            tracing::warn!(
                                scanner = name,
                                error = %e,
                                "postgres reconciler tick failed"
                            );
                        }
                    }
                }
            }
        });
    });
}
