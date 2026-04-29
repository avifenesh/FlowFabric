//! Postgres `delayed_promoter` reconciler (RFC-020 Wave 9 minimal).
//!
//! Mirrors [`ff_engine::scanner::delayed_promoter`] — promotes
//! executions parked by `attempt::delay()` back to `eligible_now`
//! once their `delay_until` wall-clock has elapsed. The Valkey side
//! scans `ff:idx:{p:N}:lane:<lane>:delayed` ZSET and fires
//! `FCALL ff_promote_delayed`; on Postgres the authority is
//! `ff_exec_core.deadline_at_ms` scoped by
//! `(lifecycle_phase = 'runnable', eligibility_state =
//! 'not_eligible_until_time')` — the tuple
//! `attempt::delay()` writes (`crates/ff-backend-postgres/src/attempt.rs:795`).
//!
//! The `deadline_at_ms` column is overloaded:
//!   * delayed rows: `deadline_at_ms = delay_until_ms`, with
//!     `eligibility_state = 'not_eligible_until_time'`.
//!   * active rows: `deadline_at_ms = execution_deadline_at_ms`, with
//!     `lifecycle_phase = 'active'` (see `execution_deadline` module).
//!
//! The two scanners are disjoint because of the
//! `(lifecycle_phase, eligibility_state)` discriminant. A dedicated
//! `delay_until_ms` column is a separate, post-v0.12 schema cleanup
//! (additive migration) — not in scope for the PR-7b minimum.
//!
//! Per-row tx: each promotion (clear `deadline_at_ms` + flip
//! `eligibility_state` → `'eligible_now'`) is its own
//! `BEGIN/COMMIT`, per spec §Constraints.

use ff_core::engine_error::EngineError;
use sqlx::{PgPool, Row};

use crate::error::map_sqlx_error;

/// Per-execution promote action. Exposed so the
/// `EngineBackend::promote_delayed` trait dispatch (PR-7b Cluster 1)
/// can invoke the same per-row tx logic the batch scanner would.
/// Silently no-ops when the exec is no longer delayed or its
/// `delay_until` has not yet elapsed (re-checked under `FOR UPDATE`).
pub async fn promote_for_execution(
    pool: &PgPool,
    partition_key: i16,
    exec_uuid: uuid::Uuid,
    now_ms: i64,
) -> Result<(), EngineError> {
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    let row = sqlx::query(
        r#"
        SELECT lifecycle_phase, eligibility_state, deadline_at_ms
          FROM ff_exec_core
         WHERE partition_key = $1 AND execution_id = $2
         FOR UPDATE
        "#,
    )
    .bind(partition_key)
    .bind(exec_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(row) = row else {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(());
    };
    let phase: String = row.try_get("lifecycle_phase").map_err(map_sqlx_error)?;
    let elig: String = row.try_get("eligibility_state").map_err(map_sqlx_error)?;
    let delay_until: Option<i64> = row
        .try_get::<Option<i64>, _>("deadline_at_ms")
        .map_err(map_sqlx_error)?;

    if phase != "runnable"
        || elig != "not_eligible_until_time"
        || !matches!(delay_until, Some(t) if t <= now_ms)
    {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(());
    }

    sqlx::query(
        r#"
        UPDATE ff_exec_core
           SET eligibility_state = 'eligible_now',
               deadline_at_ms    = NULL
         WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(partition_key)
    .bind(exec_uuid)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(())
}
