//! Postgres `suspension_timeout` reconciler (wave 6c).
//!
//! Mirrors [`ff_engine::scanner::suspension_timeout`] — scans
//! `ff_suspension_current` for active suspensions whose `timeout_at_ms`
//! has lapsed and applies the configured [`TimeoutBehavior`] recorded
//! at suspend-time (migration 0005 added the column).
//!
//! Per-row tx. Behaviors honored today:
//!   * `Fail` (default when column is NULL) — exec terminal-failed +
//!     completion event emitted.
//!   * `AutoResumeWithTimeoutSignal` — append a synthetic "timeout"
//!     signal into `ff_suspension_current.member_map` under key
//!     `__timeout__` + clear `timeout_at_ms`, keeping the exec in its
//!     suspended state so a subsequent `deliver_signal` /
//!     `claim_resumed_execution` drains it. (The RFC-004 full
//!     semantics also re-evaluate the composite condition; that
//!     plumbing is Valkey-parity work for a later wave.)
//!   * `Cancel` / `Expire` / `Escalate` — treated as terminal-cancel
//!     with distinct `cancellation_reason`; no further fan-out.

use ff_core::backend::ScannerFilter;
use ff_core::engine_error::EngineError;
use serde_json::{Value as JsonValue, json};
use sqlx::{PgPool, Row};

use crate::error::map_sqlx_error;
use crate::reconcilers::ScanReport;
use crate::reconcilers::attempt_timeout::skip_by_filter;

const BATCH_SIZE: i64 = 1000;

/// Scan one partition for expired suspensions.
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

    let rows = sqlx::query(
        r#"
        SELECT execution_id, timeout_behavior
          FROM ff_suspension_current
         WHERE partition_key = $1
           AND timeout_at_ms IS NOT NULL
           AND timeout_at_ms < $2
         ORDER BY timeout_at_ms ASC
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
        let behavior: Option<String> = row
            .try_get::<Option<String>, _>("timeout_behavior")
            .map_err(map_sqlx_error)?;
        if skip_by_filter(pool, partition_key, exec_uuid, filter).await {
            continue;
        }
        let b = behavior.as_deref().unwrap_or("fail");
        match expire_one(pool, partition_key, exec_uuid, b, now_ms).await {
            Ok(()) => report.processed += 1,
            Err(e) => {
                tracing::warn!(
                    partition = partition_key,
                    execution = %exec_uuid,
                    behavior = b,
                    error = %e,
                    "suspension_timeout reconciler: expire failed",
                );
                report.errors += 1;
            }
        }
    }
    Ok(report)
}

async fn expire_one(
    pool: &PgPool,
    partition_key: i16,
    exec_uuid: uuid::Uuid,
    behavior: &str,
    now_ms: i64,
) -> Result<(), EngineError> {
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    // Lock the suspension row and re-check the deadline. If another
    // path (deliver_signal → resume) resolved the suspension between
    // scan and action, timeout_at_ms will be NULL or the row gone.
    let row = sqlx::query(
        r#"
        SELECT timeout_at_ms, member_map
          FROM ff_suspension_current
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
    let timeout_at: Option<i64> = row
        .try_get::<Option<i64>, _>("timeout_at_ms")
        .map_err(map_sqlx_error)?;
    if !matches!(timeout_at, Some(t) if t < now_ms) {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(());
    }

    match behavior {
        "auto_resume_with_timeout_signal" => {
            // Append synthetic timeout signal into member_map[__timeout__]
            // and clear timeout_at_ms so the scanner doesn't re-fire.
            // The signal_name is fixed to "timeout" — consumers that
            // want a custom name thread it through the composite
            // condition's `on_timeout` branch (future wave).
            let member_map: JsonValue = row.try_get("member_map").map_err(map_sqlx_error)?;
            let mut map = match member_map {
                JsonValue::Object(m) => m,
                _ => serde_json::Map::new(),
            };
            let entry = map
                .entry("__timeout__".to_string())
                .or_insert_with(|| JsonValue::Array(Vec::new()));
            if let JsonValue::Array(a) = entry {
                a.push(json!({
                    "signal_name": "timeout",
                    "payload": null,
                    "delivered_at_ms": now_ms,
                    "source": "suspension_timeout",
                }));
            }
            sqlx::query(
                r#"
                UPDATE ff_suspension_current
                   SET member_map    = $1,
                       timeout_at_ms = NULL
                 WHERE partition_key = $2 AND execution_id = $3
                "#,
            )
            .bind(JsonValue::Object(map))
            .bind(partition_key)
            .bind(exec_uuid)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;
        }
        // "fail" (default), "cancel", "expire", "escalate": all
        // collapse to terminal in the PG port. Differentiate the
        // `cancellation_reason` so downstream audit can tell them
        // apart.
        other => {
            let reason = match other {
                "cancel" => "suspension_timeout_cancel",
                "expire" => "suspension_timeout_expire",
                "escalate" => "suspension_timeout_escalate",
                _ => "suspension_timeout_fail",
            };
            sqlx::query(
                r#"
                UPDATE ff_exec_core
                   SET lifecycle_phase = 'terminal',
                       ownership_state = 'unowned',
                       eligibility_state = 'not_applicable',
                       attempt_state = 'attempt_terminal',
                       terminal_at_ms = $1,
                       cancellation_reason = $2,
                       raw_fields = raw_fields || jsonb_build_object(
                           'last_failure_message', 'suspension_timeout'
                       )
                 WHERE partition_key = $3 AND execution_id = $4
                "#,
            )
            .bind(now_ms)
            .bind(reason)
            .bind(partition_key)
            .bind(exec_uuid)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;

            // Clear the suspension row — no longer pending.
            sqlx::query(
                r#"
                DELETE FROM ff_suspension_current
                 WHERE partition_key = $1 AND execution_id = $2
                "#,
            )
            .bind(partition_key)
            .bind(exec_uuid)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;

            // Outbox: emit completion event.
            let outcome = if other == "cancel" {
                "cancelled"
            } else {
                "expired"
            };
            sqlx::query(
                r#"
                INSERT INTO ff_completion_event (
                    partition_key, execution_id, flow_id, outcome,
                    namespace, instance_tag, occurred_at_ms
                )
                SELECT partition_key, execution_id, flow_id, $3,
                       NULL, NULL, $4
                  FROM ff_exec_core
                 WHERE partition_key = $1 AND execution_id = $2
                "#,
            )
            .bind(partition_key)
            .bind(exec_uuid)
            .bind(outcome)
            .bind(now_ms)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;
        }
    }

    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(())
}
