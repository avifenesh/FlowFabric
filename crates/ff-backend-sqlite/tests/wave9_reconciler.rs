//! RFC-023 Phase 3.5 — scanner supervisor (N=1) + budget_reset
//! reconciler integration tests.
//!
//! Covers:
//!   * `budget_reset` reconciler fires on a past-due row and
//!     zeroes usage + advances `next_reset_at_ms`.
//!   * `budget_reset` reconciler skips rows whose
//!     `next_reset_at_ms` is still in the future.
//!   * Supervisor spawn + explicit `shutdown()` returns cleanly
//!     (no timed-out tasks under the default grace).
//!   * Supervisor drives the reconciler on cadence — after
//!     several short ticks a past-due row becomes reset.

#![cfg(feature = "core")]

use std::sync::Arc;
use std::time::Duration;

use ff_backend_sqlite::{SqliteBackend, SqliteScannerConfig};
use ff_core::contracts::{CreateBudgetArgs, ReportUsageAdminArgs};
use ff_core::engine_backend::EngineBackend;
use ff_core::types::{BudgetId, TimestampMs};
use serial_test::serial;

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(0)
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tid = std::thread::current().id();
    format!("{ns}-{tid:?}").replace([':', ' '], "-")
}

async fn fresh_backend(tag: &str) -> Arc<SqliteBackend> {
    // SAFETY: test-only env; serial-gated.
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    let uri = format!(
        "file:rfc-023-wave9-reconciler-{tag}-{}?mode=memory&cache=shared",
        uuid_like()
    );
    SqliteBackend::new(&uri).await.expect("backend")
}

fn budget_args(id: &BudgetId, reset_interval_ms: u64) -> CreateBudgetArgs {
    CreateBudgetArgs {
        budget_id: id.clone(),
        scope_type: "flow".into(),
        scope_id: "flow-1".into(),
        enforcement_mode: "strict".into(),
        on_hard_limit: "fail".into(),
        on_soft_limit: "warn".into(),
        reset_interval_ms,
        dimensions: vec!["tokens".into()],
        hard_limits: vec![1000],
        soft_limits: vec![800],
        now: TimestampMs(now_ms()),
    }
}

fn admin_args(dim: &str, delta: u64) -> ReportUsageAdminArgs {
    ReportUsageAdminArgs::new(vec![dim.into()], vec![delta], TimestampMs(now_ms()))
}

/// Force a budget's `next_reset_at_ms` to a specific value — emulates
/// "time passed" without requiring wall-clock sleep. Reaches through
/// the test-only pool accessor.
async fn force_next_reset(b: &SqliteBackend, budget_id: &BudgetId, next_reset_at_ms: Option<i64>) {
    let pool = b.pool_for_test();
    sqlx::query("UPDATE ff_budget_policy SET next_reset_at_ms = ? WHERE partition_key = 0 AND budget_id = ?")
        .bind(next_reset_at_ms)
        .bind(budget_id.to_string())
        .execute(pool)
        .await
        .expect("force next_reset_at_ms");
}

// ── Reconciler behaviour ───────────────────────────────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn budget_reset_reconciler_fires_on_due_row() {
    let b = fresh_backend("fires").await;
    let bid = BudgetId::new();
    let interval_ms: u64 = 60_000;

    b.create_budget(budget_args(&bid, interval_ms))
        .await
        .unwrap();
    b.report_usage_admin(&bid, admin_args("tokens", 200))
        .await
        .unwrap();

    // Pull the budget's schedule "into the past" so the reconciler
    // treats it as due.
    let t_now = now_ms();
    force_next_reset(&b, &bid, Some(t_now - 1_000)).await;

    let pre = b.get_budget_status(&bid).await.unwrap();
    assert_eq!(pre.usage.get("tokens").copied(), Some(200));

    // Drive one reconciler tick at `t_now`.
    let (processed, errors) = b
        .budget_reset_scan_tick_for_test(t_now)
        .await
        .expect("scan_tick");
    assert_eq!(errors, 0, "no reconciler errors expected");
    assert_eq!(processed, 1, "one due row processed");

    let post = b.get_budget_status(&bid).await.unwrap();
    assert_eq!(
        post.usage.get("tokens").copied(),
        Some(0),
        "usage zeroed by reconciler"
    );
    let next_post: i64 = post
        .next_reset_at
        .as_deref()
        .expect("reconciler must reschedule next_reset_at")
        .parse()
        .expect("parseable");
    assert!(
        next_post >= t_now + interval_ms as i64,
        "next_reset_at advanced by at least one interval (got {next_post}, expected >= {})",
        t_now + interval_ms as i64
    );
    assert!(post.last_breach_at.is_none(), "last_breach_at cleared");
    assert!(post.last_breach_dim.is_none(), "last_breach_dim cleared");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn budget_reset_reconciler_skips_not_due() {
    let b = fresh_backend("skips").await;
    let bid = BudgetId::new();
    let interval_ms: u64 = 60_000;

    b.create_budget(budget_args(&bid, interval_ms))
        .await
        .unwrap();
    b.report_usage_admin(&bid, admin_args("tokens", 150))
        .await
        .unwrap();

    // Force the schedule well into the future.
    let t_now = now_ms();
    let future = t_now + 10 * interval_ms as i64;
    force_next_reset(&b, &bid, Some(future)).await;

    let (processed, errors) = b
        .budget_reset_scan_tick_for_test(t_now)
        .await
        .expect("scan_tick");
    assert_eq!(errors, 0);
    assert_eq!(processed, 0, "future-due row must not be processed");

    let post = b.get_budget_status(&bid).await.unwrap();
    assert_eq!(
        post.usage.get("tokens").copied(),
        Some(150),
        "usage unchanged when not due"
    );
    assert_eq!(
        post.next_reset_at.as_deref().map(|s| s.parse::<i64>().unwrap()),
        Some(future),
        "next_reset_at unchanged when not due"
    );
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn budget_reset_reconciler_skips_null_schedule() {
    // Budgets with `reset_interval_ms = 0` carry NULL
    // `next_reset_at_ms` — the partial index excludes them; the
    // reconciler must not touch them.
    let b = fresh_backend("null-schedule").await;
    let bid = BudgetId::new();
    b.create_budget(budget_args(&bid, 0)).await.unwrap();
    b.report_usage_admin(&bid, admin_args("tokens", 42))
        .await
        .unwrap();

    let (processed, errors) = b
        .budget_reset_scan_tick_for_test(now_ms() + 10_000_000)
        .await
        .expect("scan_tick");
    assert_eq!(errors, 0);
    assert_eq!(processed, 0, "NULL-scheduled row never due");

    let post = b.get_budget_status(&bid).await.unwrap();
    assert_eq!(post.usage.get("tokens").copied(), Some(42));
    assert!(post.next_reset_at.is_none());
}

// ── Supervisor lifecycle ────────────────────────────────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn supervisor_starts_and_stops_cleanly() {
    let b = fresh_backend("start-stop").await;
    let installed = b.with_scanners(SqliteScannerConfig {
        budget_reset_interval: Duration::from_millis(100),
    });
    assert!(installed, "first with_scanners call installs supervisor");

    // Second call is a no-op (idempotent).
    let again = b.with_scanners(SqliteScannerConfig {
        budget_reset_interval: Duration::from_millis(100),
    });
    assert!(!again, "second with_scanners call must be a no-op");

    // Graceful drain.
    b.shutdown_prepare(Duration::from_secs(2))
        .await
        .expect("shutdown_prepare");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn supervisor_shutdown_drains_immediately_after_install() {
    // Regression guard: earlier drafts spawned tick tasks from a
    // detached `tokio::spawn` acquiring an async mutex, which left a
    // window where `shutdown()` could observe an empty JoinSet while
    // the task was still queued. The final shape spawns synchronously
    // during `spawn_scanners`, so an immediate shutdown MUST drain
    // the task (timed_out == 0).
    let b = fresh_backend("immediate-shutdown").await;
    b.with_scanners(SqliteScannerConfig {
        // 1h cadence — the task is parked at `select!`, not doing work.
        budget_reset_interval: Duration::from_secs(3600),
    });
    b.shutdown_prepare(Duration::from_secs(2))
        .await
        .expect("shutdown_prepare drains registered tasks");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn supervisor_zero_interval_is_disabled_not_panic() {
    // `tokio::time::interval(Duration::ZERO)` panics; the supervisor
    // must treat zero as "reconciler disabled" per
    // FF_BUDGET_RESET_INTERVAL_S=0 semantics.
    let b = fresh_backend("zero-interval").await;
    b.with_scanners(SqliteScannerConfig {
        budget_reset_interval: Duration::from_millis(0),
    });
    // Shutdown should still work and report zero timed-out tasks.
    b.shutdown_prepare(Duration::from_secs(1))
        .await
        .expect("zero-interval supervisor must shut down cleanly");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn supervisor_tick_advances_on_cadence() {
    let b = fresh_backend("cadence").await;
    let bid = BudgetId::new();
    let interval_ms: u64 = 60_000;
    b.create_budget(budget_args(&bid, interval_ms))
        .await
        .unwrap();
    b.report_usage_admin(&bid, admin_args("tokens", 300))
        .await
        .unwrap();
    force_next_reset(&b, &bid, Some(now_ms() - 1_000)).await;

    // Short cadence to force at least one tick inside the test.
    b.with_scanners(SqliteScannerConfig {
        budget_reset_interval: Duration::from_millis(50),
    });

    // Wait up to ~3s for the supervisor to notice the due row.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut observed_reset = false;
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let s = b.get_budget_status(&bid).await.unwrap();
        if s.usage.get("tokens").copied() == Some(0) {
            observed_reset = true;
            break;
        }
    }
    assert!(
        observed_reset,
        "supervisor must reset the due budget within the cadence window"
    );

    b.shutdown_prepare(Duration::from_secs(2))
        .await
        .expect("shutdown_prepare");
}
