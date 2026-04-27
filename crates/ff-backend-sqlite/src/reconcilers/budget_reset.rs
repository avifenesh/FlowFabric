//! SQLite `budget_reset` reconciler (RFC-023 Phase 3.5 /
//! RFC-020 Wave 9 Standalone-1, §4.4.3 Gap 2 Option B).
//!
//! Ports `ff-backend-postgres::reconcilers::budget_reset` to SQLite.
//! Scans `ff_budget_policy` for rows whose `next_reset_at_ms` has
//! lapsed (backed by the `ff_budget_policy_reset_due_idx` partial
//! index from migration 0013) and invokes the shared
//! [`crate::budget::budget_reset_reconciler_apply`] helper — same
//! code path as the admin `reset_budget` FCALL's body.
//!
//! Fan-out: N=1. SQLite has one (logical) partition (§4.1
//! `num_budget_partitions = 1`), so there is no outer partition
//! loop — the supervisor calls `scan_tick(pool, now_ms)` once per
//! cadence tick.
//!
//! Race discipline: a manual `reset_budget` and the reconciler may
//! target the same budget concurrently. Both run under
//! `BEGIN IMMEDIATE` and re-read `next_reset_at_ms` inside the tx.
//! If a concurrent admin reset has already advanced the schedule
//! past `now`, [`crate::budget::budget_reset_reconciler_apply`]
//! skips applying another reset; otherwise it performs the same
//! reset transition as the admin path. Deterministic under the
//! single-writer invariant.

use sqlx::{Row, SqlitePool};

use ff_core::engine_error::EngineError;

use crate::budget;
use crate::errors::map_sqlx_error;
use crate::reconcilers::ScanReport;

/// Partial-index-backed bounded batch. Matches the PG reference
/// (`BATCH_SIZE = 20`).
const BATCH_SIZE: i64 = 20;

/// Single-partition constant — see `crate::budget` module docs.
const PART: i64 = 0;

/// Scan for budgets with a lapsed `next_reset_at_ms` and apply the
/// reset transaction per hit. `now_ms` is the wall-clock supplied by
/// the supervisor tick (so tests can drive a fixed `now`).
pub async fn scan_tick(pool: &SqlitePool, now_ms: i64) -> Result<ScanReport, EngineError> {
    // Bounded batch scan. Backed by `ff_budget_policy_reset_due_idx`
    // (partial). `ORDER BY next_reset_at_ms` keeps earliest-due
    // first, matching the PG reference + Valkey ZRANGEBYSCORE order.
    let rows = sqlx::query(
        "SELECT budget_id \
           FROM ff_budget_policy \
          WHERE partition_key = ? \
            AND next_reset_at_ms IS NOT NULL \
            AND next_reset_at_ms <= ? \
          ORDER BY next_reset_at_ms ASC \
          LIMIT ?",
    )
    .bind(PART)
    .bind(now_ms)
    .bind(BATCH_SIZE)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx_error)?;

    let mut report = ScanReport::default();
    for row in rows {
        let budget_id: String = row.try_get("budget_id").map_err(map_sqlx_error)?;
        match budget::budget_reset_reconciler_apply(pool, &budget_id, now_ms).await {
            Ok(()) => report.processed += 1,
            Err(e) => {
                tracing::warn!(
                    budget_id = %budget_id,
                    error = %e,
                    "sqlite budget_reset reconciler: reset failed",
                );
                report.errors += 1;
            }
        }
    }
    Ok(report)
}
