//! Postgres `execution_deadline` reconciler (RFC-020 Wave 9 minimal).
//!
//! Mirrors the `ExecutionDeadline` phase of
//! [`ff_engine::scanner::execution_deadline`] — terminates executions
//! whose whole-execution wall-clock deadline has elapsed. Shares a
//! trait entry point (`expire_execution`) with `attempt_timeout`;
//! discriminated by [`ff_core::engine_backend::ExpirePhase`].
//!
//! Candidate selection: `(lifecycle_phase = 'active', deadline_at_ms
//! IS NOT NULL, deadline_at_ms < now)` on `ff_exec_core`. The
//! `deadline_at_ms` column is overloaded (it also carries
//! `delay_until` for rows parked by `attempt::delay()`); the
//! `lifecycle_phase = 'active'` predicate discriminates.
//!
//! Action: terminal-fail with `last_failure_message =
//! 'execution_deadline'`, clear the active attempt's lease, emit
//! `ff_completion_event { outcome = 'expired' }`.

use ff_core::engine_error::EngineError;
use sqlx::{PgPool, Row};

use crate::error::map_sqlx_error;
use crate::lease_event;

/// Per-execution expire action for the execution-deadline scanner.
/// Exposed so the `EngineBackend::expire_execution` trait dispatch
/// (`ExpirePhase::ExecutionDeadline`) can invoke the same per-row tx
/// logic the batch scanner would. Silently no-ops when the exec is
/// no longer `active` or its deadline has not yet elapsed (re-checked
/// under `FOR UPDATE`).
pub async fn expire_for_execution(
    pool: &PgPool,
    partition_key: i16,
    exec_uuid: uuid::Uuid,
    now_ms: i64,
) -> Result<(), EngineError> {
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    let core_row = sqlx::query(
        r#"
        SELECT attempt_index, lifecycle_phase, deadline_at_ms
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

    let Some(core) = core_row else {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(());
    };
    let phase: String = core.try_get("lifecycle_phase").map_err(map_sqlx_error)?;
    let deadline_at: Option<i64> = core
        .try_get::<Option<i64>, _>("deadline_at_ms")
        .map_err(map_sqlx_error)?;
    if phase != "active" || !matches!(deadline_at, Some(d) if d < now_ms) {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(());
    }
    let attempt_index: i32 = core.try_get("attempt_index").map_err(map_sqlx_error)?;

    // Release the active attempt's lease + mark it interrupted. The
    // row may legitimately lack a lease (e.g. exec is active but its
    // current attempt is mid-transition) — UPDATE affecting 0 rows is
    // fine.
    sqlx::query(
        r#"
        UPDATE ff_attempt
           SET outcome = 'interrupted',
               lease_expires_at_ms = NULL,
               terminal_at_ms = $1
         WHERE partition_key = $2 AND execution_id = $3 AND attempt_index = $4
        "#,
    )
    .bind(now_ms)
    .bind(partition_key)
    .bind(exec_uuid)
    .bind(attempt_index)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    sqlx::query(
        r#"
        UPDATE ff_exec_core
           SET lifecycle_phase = 'terminal',
               ownership_state = 'unowned',
               eligibility_state = 'not_applicable',
               attempt_state = 'attempt_terminal',
               terminal_at_ms = $1,
               raw_fields = raw_fields || jsonb_build_object(
                   'last_failure_message', 'execution_deadline'
               )
         WHERE partition_key = $2 AND execution_id = $3
        "#,
    )
    .bind(now_ms)
    .bind(partition_key)
    .bind(exec_uuid)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    sqlx::query(
        r#"
        INSERT INTO ff_completion_event (
            partition_key, execution_id, flow_id, outcome,
            namespace, instance_tag, occurred_at_ms
        )
        SELECT partition_key, execution_id, flow_id, 'expired',
               NULL, NULL, $3
          FROM ff_exec_core
         WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(partition_key)
    .bind(exec_uuid)
    .bind(now_ms)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // RFC-019 Stage B outbox: lease expired (execution_deadline
    // reconciler). Matches the `attempt_timeout` terminal emit.
    lease_event::emit(
        &mut tx,
        partition_key,
        exec_uuid,
        None,
        lease_event::EVENT_EXPIRED,
        now_ms,
    )
    .await?;

    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(())
}
