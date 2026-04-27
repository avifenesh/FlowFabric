//! SQLite scanner supervisor (RFC-023 Phase 3.5).
//!
//! SQLite collapses the Postgres per-partition fan-out to N=1
//! (§4.1): one process, one writer, one logical partition. The
//! supervisor is therefore just a tokio task per reconciler type
//! running a `tokio::time::interval` tick loop with a `watch`
//! shutdown channel — no partition iteration.
//!
//! Phase 3.5 ships only `budget_reset`. Future reconcilers (if the
//! §4.1 A3 single-writer envelope ever needs them) drop in as
//! additional `spawn_reconciler` calls here.
//!
//! # Shutdown
//!
//! [`SqliteScannerHandle::shutdown`] flips the `watch` channel and
//! awaits the underlying [`JoinSet`] up to `grace`. Tasks are
//! registered synchronously during [`spawn_scanners`] (before the
//! handle is returned), so the drain contract holds even for an
//! immediate shutdown after `with_scanners`. Mirrors the
//! `PostgresScannerHandle::shutdown` shape so `ff-server`'s
//! `Server::shutdown` drain logic stays backend-agnostic.

use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinSet;

use crate::reconcilers;
use crate::tx_util::now_ms;

/// Subset of `EngineConfig`'s interval knobs that the SQLite
/// reconcilers honour. Only `budget_reset_interval` is wired today;
/// kept as a struct (not a bare `Duration`) so additional cadences
/// are additive, matching [`ff_backend_postgres::PostgresScannerConfig`].
#[derive(Clone, Debug)]
pub struct SqliteScannerConfig {
    /// RFC-020 Wave 9 Standalone-1 cadence. Matches the Valkey side's
    /// `ff-server::config::budget_reset_interval` knob so one env
    /// value drives all three backends.
    ///
    /// If set to zero (`FF_BUDGET_RESET_INTERVAL_S=0`) the reconciler
    /// is treated as disabled and not spawned, mirroring
    /// `tokio::time::interval`'s zero-duration panic safety.
    pub budget_reset_interval: Duration,
}

/// Spawned scanner supervisor. Holding this handle keeps the
/// reconciler tasks alive; drop-on-backend or explicit shutdown
/// drains them. Returned by [`spawn_scanners`]; stored inside
/// `SqliteBackendInner` so `EngineBackend::shutdown_prepare` can
/// drain on server shutdown.
pub struct SqliteScannerHandle {
    shutdown_tx: watch::Sender<bool>,
    join_set: Arc<Mutex<JoinSet<()>>>,
}

impl SqliteScannerHandle {
    /// Signal shutdown and await drain up to `grace`. Returns the
    /// number of tasks that did not exit cleanly within the grace
    /// window (for operator logging). Subsequent calls are no-ops.
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

impl Drop for SqliteScannerHandle {
    /// Best-effort signal on drop. Tasks exit at their next tick; if
    /// the caller wants a bounded drain they must call
    /// [`Self::shutdown`] explicitly (per PG parity).
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
    }
}

/// Spawn all SQLite reconcilers as long-lived tick loops. Phase 3.5
/// ships one: `budget_reset`. Tasks are registered into the
/// `JoinSet` synchronously — the handle returned is always drainable
/// via [`SqliteScannerHandle::shutdown`].
pub fn spawn_scanners(pool: SqlitePool, cfg: SqliteScannerConfig) -> SqliteScannerHandle {
    let (tx, rx) = watch::channel(false);
    let mut js = JoinSet::new();

    // Zero-interval guard: treat as disabled rather than panicking
    // in `tokio::time::interval`. Matches `FF_BUDGET_RESET_INTERVAL_S=0`
    // as an opt-out.
    if !cfg.budget_reset_interval.is_zero() {
        spawn_reconciler(
            &mut js,
            rx.clone(),
            pool.clone(),
            cfg.budget_reset_interval,
            "sqlite.budget_reset",
            |pool| {
                Box::pin(async move {
                    reconcilers::budget_reset::scan_tick(&pool, now_ms())
                        .await
                        .map(|_| ())
                })
            },
        );
    }

    let scanners = js.len();
    tracing::info!(
        scanners,
        "sqlite scanner supervisor spawned (RFC-023 Phase 3.5 — budget_reset N=1)"
    );

    SqliteScannerHandle {
        shutdown_tx: tx,
        join_set: Arc::new(Mutex::new(js)),
    }
}

type TickFut = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(), ff_core::engine_error::EngineError>> + Send>,
>;

/// Register one reconciler tick task synchronously into the
/// supervisor's [`JoinSet`]. Tasks exit at the next `select!` once
/// `shutdown` fires (or the watch channel is closed).
fn spawn_reconciler<F>(
    js: &mut JoinSet<()>,
    mut shutdown: watch::Receiver<bool>,
    pool: SqlitePool,
    interval: Duration,
    name: &'static str,
    tick: F,
) where
    F: Fn(SqlitePool) -> TickFut + Send + Sync + 'static,
{
    js.spawn(async move {
        let mut tk = tokio::time::interval(interval);
        tk.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // `tokio::time::interval` yields an immediate first tick;
        // drain it so the first observable reconciler run happens
        // `interval` after spawn. Intentional SQLite startup-timing
        // difference from the Postgres supervisor, which lets the
        // first immediate tick fire. Safe: the watch shutdown signal
        // is only delivered via `changed()`, so skipping this
        // pre-drain tick has no shutdown-observability cost.
        tk.tick().await;
        loop {
            tokio::select! {
                res = shutdown.changed() => {
                    // Channel closed (sender dropped) OR shutdown=true → exit.
                    if res.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
                _ = tk.tick() => {
                    if *shutdown.borrow() {
                        return;
                    }
                    if let Err(e) = tick(pool.clone()).await {
                        tracing::warn!(
                            scanner = name,
                            error = %e,
                            "sqlite reconciler tick failed"
                        );
                    }
                }
            }
        }
    });
}
