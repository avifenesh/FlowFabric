//! Postgres `lease_expiry` reconciler (wave 6c).
//!
//! Mirrors [`ff_engine::scanner::lease_expiry`] — scans for attempts
//! whose lease has lapsed and marks them reclaimable so another worker
//! can redeem the execution via [`crate::attempt::claim_from_reclaim`].
//!
//! Action per row (in its own tx):
//!   * NULL out `ff_attempt.lease_expires_at_ms` (the fence-triple
//!     release — `claim_from_reclaim` requires either
//!     `lease_expires_at_ms <= now` OR `NULL` to succeed).
//!   * Set `ff_attempt.outcome = 'lease_expired_reclaimable'`.
//!   * Flip `ff_exec_core` to `ownership_state = 'unowned'` +
//!     `lifecycle_phase = 'runnable'` + `eligibility_state =
//!     'eligible_now'` + `attempt_state = 'attempt_interrupted'` so
//!     the normal claim path picks it back up (or a targeted
//!     `claim_from_reclaim` redeems the specific attempt).
//!
//! Distinction from `attempt_timeout`: `attempt_timeout` terminates
//! or retries the whole execution (worker vanished AND retry budget
//! is the authority). `lease_expiry` is the gentler reclaim path —
//! keeps the same attempt_index, just releases the lease so a new
//! worker can take over. The Valkey side runs these as two separate
//! scanners for the same reason; whichever fires first on a given
//! execution wins.

use ff_core::backend::ScannerFilter;
use ff_core::engine_error::EngineError;
use sqlx::{PgPool, Row};

use crate::error::map_sqlx_error;
use crate::reconcilers::ScanReport;
use crate::reconcilers::attempt_timeout::skip_by_filter;

const BATCH_SIZE: i64 = 1000;

/// Scan one partition for expired-lease attempts and emit reclaimable
/// markers.
pub async fn scan_tick(
    pool: &PgPool,
    partition_key: i16,
    filter: &ScannerFilter,
) -> Result<ScanReport, EngineError> {
    let now_ms: i64 = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(i64::MAX);

    // Candidate rows: any attempt with a lapsed lease, regardless of
    // lifecycle phase. Unlike `attempt_timeout` we DO NOT join on
    // `lifecycle_phase = 'active'` — a crashed worker that left the
    // lease behind may have already been flipped by a different scanner
    // or by cancel_flow; we still want to zero out the stale lease
    // column so `claim_from_reclaim` cleanly sees a released slot.
    let rows = sqlx::query(
        r#"
        SELECT execution_id, attempt_index
          FROM ff_attempt
         WHERE partition_key = $1
           AND lease_expires_at_ms IS NOT NULL
           AND lease_expires_at_ms < $2
         ORDER BY lease_expires_at_ms ASC
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
        let exec_uuid: uuid::Uuid = row.try_get("execution_id").map_err(map_sqlx_error)?;
        let attempt_index: i32 = row.try_get("attempt_index").map_err(map_sqlx_error)?;
        if skip_by_filter(pool, partition_key, exec_uuid, filter).await {
            continue;
        }
        match release_one(pool, partition_key, exec_uuid, attempt_index, now_ms).await {
            Ok(()) => report.processed += 1,
            Err(e) => {
                tracing::warn!(
                    partition = partition_key,
                    execution = %exec_uuid,
                    attempt_index,
                    error = %e,
                    "lease_expiry reconciler: row release failed",
                );
                report.errors += 1;
            }
        }
    }
    Ok(report)
}

async fn release_one(
    pool: &PgPool,
    partition_key: i16,
    exec_uuid: uuid::Uuid,
    attempt_index: i32,
    now_ms: i64,
) -> Result<(), EngineError> {
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    // Re-read under lock — defensive against a worker that renewed
    // its lease between scan and action.
    let att_row = sqlx::query(
        r#"
        SELECT lease_expires_at_ms
          FROM ff_attempt
         WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = $3
         FOR UPDATE
        "#,
    )
    .bind(partition_key)
    .bind(exec_uuid)
    .bind(attempt_index)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(att) = att_row else {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(());
    };
    let expires_at: Option<i64> = att
        .try_get::<Option<i64>, _>("lease_expires_at_ms")
        .map_err(map_sqlx_error)?;
    if !matches!(expires_at, Some(e) if e < now_ms) {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(());
    }

    sqlx::query(
        r#"
        UPDATE ff_attempt
           SET lease_expires_at_ms = NULL,
               outcome = 'lease_expired_reclaimable'
         WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = $3
        "#,
    )
    .bind(partition_key)
    .bind(exec_uuid)
    .bind(attempt_index)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // Flip exec_core to reclaimable-runnable. Only touch rows still
    // in `active` — a terminal/cancelled exec should be left alone.
    sqlx::query(
        r#"
        UPDATE ff_exec_core
           SET lifecycle_phase = 'runnable',
               ownership_state = 'unowned',
               eligibility_state = 'eligible_now',
               attempt_state = 'attempt_interrupted',
               raw_fields = raw_fields || jsonb_build_object(
                   'lease_expired_at_ms', $3::bigint
               )
         WHERE partition_key = $1 AND execution_id = $2
           AND lifecycle_phase = 'active'
        "#,
    )
    .bind(partition_key)
    .bind(exec_uuid)
    .bind(now_ms)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(())
}
