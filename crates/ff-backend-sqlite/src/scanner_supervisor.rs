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
//! awaits the underlying [`JoinSet`] up to `grace`. Ticks observe
//! the signal at the next `select!` and exit. Mirrors the
//! `PostgresScannerHandle::shutdown` shape so `ff-server`'s
//! `Server::shutdown` drain logic stays backend-agnostic.

use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinSet;

use crate::reconcilers;

/// Subset of `EngineConfig`'s interval knobs that the SQLite
/// reconcilers honour. Only `budget_reset_interval` is wired today;
/// kept as a struct (not a bare `Duration`) so additional cadences
/// are additive, matching [`ff_backend_postgres::PostgresScannerConfig`].
#[derive(Clone, Debug)]
pub struct SqliteScannerConfig {
    /// RFC-020 Wave 9 Standalone-1 cadence. Matches the Valkey side's
    /// `ff-server::config::budget_reset_interval` knob so one env
    /// value drives all three backends.
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
/// ships one: `budget_reset`.
pub fn spawn_scanners(pool: SqlitePool, cfg: SqliteScannerConfig) -> SqliteScannerHandle {
    let (tx, rx) = watch::channel(false);
    let js = Arc::new(Mutex::new(JoinSet::new()));

    spawn_reconciler(
        &js,
        rx,
        pool,
        cfg.budget_reset_interval,
        "sqlite.budget_reset",
        |pool| {
            Box::pin(async move {
                let now = now_ms();
                reconcilers::budget_reset::scan_tick(&pool, now)
                    .await
                    .map(|_| ())
            })
        },
    );

    tracing::info!(
        scanners = 1,
        "sqlite scanner supervisor spawned (RFC-023 Phase 3.5 — budget_reset N=1)"
    );

    SqliteScannerHandle {
        shutdown_tx: tx,
        join_set: js,
    }
}

type TickFut =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ff_core::engine_error::EngineError>> + Send>>;

fn spawn_reconciler<F>(
    js: &Arc<Mutex<JoinSet<()>>>,
    mut shutdown: watch::Receiver<bool>,
    pool: SqlitePool,
    interval: Duration,
    name: &'static str,
    tick: F,
) where
    F: Fn(SqlitePool) -> TickFut + Send + Sync + 'static + Clone,
{
    let js_clone = js.clone();
    tokio::spawn(async move {
        let mut guard = js_clone.lock().await;
        guard.spawn(async move {
            let mut tk = tokio::time::interval(interval);
            tk.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // First tick fires immediately; skip it so the first
            // observable reconciler run happens `interval` after
            // spawn. Matches the PG supervisor shape (which uses the
            // same default interval behaviour).
            tk.tick().await;
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
    });
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(i64::MAX)
}
