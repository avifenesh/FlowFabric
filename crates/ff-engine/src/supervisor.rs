//! Scanner supervisor — restarts scanners on panic with backoff.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::scanner::{Scanner, ScannerRunner};

/// Spawn a supervised scanner that restarts on panic.
///
/// - Clean shutdown (via watch channel): exits normally.
/// - Panic: logs, sleeps 5s, increments restart counter, re-spawns.
pub fn supervised_spawn<S: Scanner>(
    scanner: Arc<S>,
    client: ferriskey::Client,
    num_partitions: u16,
    shutdown: watch::Receiver<bool>,
    metrics: Arc<ff_observability::Metrics>,
) -> JoinHandle<()> {
    let restart_count = Arc::new(AtomicU32::new(0));
    let name = scanner.name();

    let rc = restart_count.clone();
    tokio::spawn(async move {
        loop {
            if *shutdown.borrow() {
                return;
            }

            let handle = ScannerRunner::spawn(
                scanner.clone(),
                client.clone(),
                num_partitions,
                shutdown.clone(),
                metrics.clone(),
            );

            match handle.await {
                Ok(()) => {
                    // Clean exit — shutdown was requested
                    return;
                }
                Err(e) if e.is_panic() => {
                    let count = rc.fetch_add(1, Ordering::Relaxed) + 1;
                    tracing::error!(
                        scanner = name,
                        restart_count = count,
                        "scanner panicked — restarting in 5s"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

                    if *shutdown.borrow() {
                        return;
                    }
                }
                Err(e) => {
                    let count = rc.fetch_add(1, Ordering::Relaxed) + 1;
                    tracing::error!(
                        scanner = name,
                        restart_count = count,
                        error = %e,
                        "scanner task failed — restarting in 1s"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                    if *shutdown.borrow() {
                        return;
                    }
                }
            }
        }
    })
}
