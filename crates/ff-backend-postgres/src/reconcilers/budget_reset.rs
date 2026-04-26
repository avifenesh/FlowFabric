//! Postgres `budget_reset` reconciler (RFC-020 Wave 9 Standalone-1,
//! §4.4.3 Gap 2 Option B).
//!
//! Mirrors [`ff_engine::scanner::budget_reset`] — scans per-partition
//! for `ff_budget_policy` rows whose `next_reset_at_ms` has lapsed and
//! invokes the shared [`crate::budget::budget_reset_reconciler_apply`]
//! codepath (same body as the admin `reset_budget` FCALL). Named
//! `budget_reset` to match the Valkey scanner name so observability
//! metrics align across backends.
//!
//! Scan shape backed by the partial index
//! `ff_budget_policy_reset_due_idx (partition_key, next_reset_at_ms)
//! WHERE next_reset_at_ms IS NOT NULL` from migration 0013 — scan
//! cost scales with the scheduled-reset subset, not the full policy
//! set.
//!
//! Race discipline: a manual `reset_budget` + the reconciler's reset
//! resolve identically. Both observe `next_reset_at_ms` under `FOR
//! NO KEY UPDATE`; the second commit re-zeros (no-op) + overwrites
//! `next_reset_at_ms` with the later `now + interval`, picked up on
//! the next tick.

use sqlx::{PgPool, Row};

use ff_core::engine_error::EngineError;

use crate::budget;
use crate::error::map_sqlx_error;
use crate::reconcilers::ScanReport;

const BATCH_SIZE: i64 = 20;

/// Scan one partition for budgets with a lapsed `next_reset_at_ms`
/// and apply the reset transaction per hit.
pub async fn scan_tick(pool: &PgPool, partition_key: i16) -> Result<ScanReport, EngineError> {
    let now_ms: i64 = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(i64::MAX);

    // Bounded batch scan. Backed by `ff_budget_policy_reset_due_idx`
    // (partial). `ORDER BY next_reset_at_ms` keeps the earliest-due
    // budgets first — same prioritisation as Valkey's ZRANGEBYSCORE.
    let rows = sqlx::query(
        r#"
        SELECT budget_id
          FROM ff_budget_policy
         WHERE partition_key = $1
           AND next_reset_at_ms IS NOT NULL
           AND next_reset_at_ms <= $2
         ORDER BY next_reset_at_ms ASC
         LIMIT $3
        "#,
    )
    .bind(partition_key)
    .bind(now_ms)
    .bind(BATCH_SIZE)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx_error)?;

    let mut report = ScanReport::default();
    for row in rows {
        let budget_id: String = row.try_get("budget_id").map_err(map_sqlx_error)?;
        match budget::budget_reset_reconciler_apply(pool, partition_key, &budget_id, now_ms).await {
            Ok(()) => report.processed += 1,
            Err(e) => {
                tracing::warn!(
                    partition = partition_key,
                    budget_id = %budget_id,
                    error = %e,
                    "budget_reset reconciler: reset failed",
                );
                report.errors += 1;
            }
        }
    }
    Ok(report)
}
