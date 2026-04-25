//! Postgres `attempt_timeout` reconciler (wave 6c).
//!
//! Mirrors [`ff_engine::scanner::attempt_timeout`] against the Postgres
//! schema. The Valkey side scans `ff:idx:{p:N}:attempt_timeout` ZSET
//! for attempts whose lease timeout has lapsed and fires
//! `FCALL ff_expire_execution`. On Postgres the authority is the
//! `ff_attempt.lease_expires_at_ms` column (partial index
//! `ff_attempt_lease_expiry_idx`); an attempt is "timed out" iff its
//! lease has expired AND the `ff_exec_core.lifecycle_phase = 'active'`
//! (a released lease — `lease_expires_at_ms IS NULL` — is not a
//! timeout; a terminal attempt is also not a timeout).
//!
//! Per-row tx: each timeout action (mark attempt interrupted + transition
//! exec_core to either `runnable` (retries remain) or `terminal`
//! (exhausted) + emit completion event on terminal) is its own
//! `BEGIN/COMMIT`, per spec §Constraints.

use ff_core::backend::ScannerFilter;
use ff_core::engine_error::EngineError;
use serde_json::Value as JsonValue;
use sqlx::{PgPool, Row};

use crate::error::map_sqlx_error;
use crate::lease_event;
use crate::reconcilers::ScanReport;

/// Max rows pulled per scan tick.
const BATCH_SIZE: i64 = 1000;

/// Grace period (ms). 0 by default.
const GRACE_MS: i64 = 0;

/// Scan one partition for timed-out attempt leases.
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
    let cutoff = now_ms.saturating_sub(GRACE_MS);

    let rows = sqlx::query(
        r#"
        SELECT a.execution_id, a.attempt_index
          FROM ff_attempt a
          JOIN ff_exec_core c
            ON c.partition_key = a.partition_key
           AND c.execution_id = a.execution_id
         WHERE a.partition_key = $1
           AND a.lease_expires_at_ms IS NOT NULL
           AND a.lease_expires_at_ms < $2
           AND c.lifecycle_phase = 'active'
         ORDER BY a.lease_expires_at_ms ASC
         LIMIT $3
        "#,
    )
    .bind(partition_key)
    .bind(cutoff)
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
        match expire_one(pool, partition_key, exec_uuid, attempt_index, now_ms).await {
            Ok(()) => report.processed += 1,
            Err(e) => {
                tracing::warn!(
                    partition = partition_key,
                    execution = %exec_uuid,
                    attempt_index,
                    error = %e,
                    "attempt_timeout reconciler: row expiry failed",
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
    attempt_index: i32,
    now_ms: i64,
) -> Result<(), EngineError> {
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

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

    let core_row = sqlx::query(
        r#"
        SELECT attempt_index, lifecycle_phase, policy
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
    if phase != "active" {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(());
    }
    let cur_attempt: i32 = core.try_get("attempt_index").map_err(map_sqlx_error)?;
    let policy: Option<JsonValue> = core.try_get("policy").map_err(map_sqlx_error)?;
    let max_retries = extract_max_retries(policy.as_ref());
    let retries_remain = (cur_attempt as i64) < (max_retries as i64);

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

    if retries_remain {
        sqlx::query(
            r#"
            UPDATE ff_exec_core
               SET lifecycle_phase = 'runnable',
                   ownership_state = 'unowned',
                   eligibility_state = 'eligible_now',
                   attempt_state = 'pending_retry_attempt',
                   attempt_index = attempt_index + 1,
                   raw_fields = raw_fields || jsonb_build_object(
                       'last_failure_message', 'attempt_timeout'
                   )
             WHERE partition_key = $1 AND execution_id = $2
            "#,
        )
        .bind(partition_key)
        .bind(exec_uuid)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
    } else {
        sqlx::query(
            r#"
            UPDATE ff_exec_core
               SET lifecycle_phase = 'terminal',
                   ownership_state = 'unowned',
                   eligibility_state = 'not_applicable',
                   attempt_state = 'attempt_terminal',
                   terminal_at_ms = $1,
                   raw_fields = raw_fields || jsonb_build_object(
                       'last_failure_message', 'attempt_timeout'
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
    }

    // RFC-019 Stage B outbox: lease expired (attempt_timeout reconciler).
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

/// Read `policy.retry_policy.max_retries` out of exec_core `policy`
/// jsonb. Defaults to 0 (no retries) when absent.
fn extract_max_retries(policy: Option<&JsonValue>) -> u32 {
    policy
        .and_then(|v| v.get("retry_policy"))
        .and_then(|rp| rp.get("max_retries"))
        .and_then(|m| m.as_u64())
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0)
}

/// Apply [`ScannerFilter`] dimensions against `ff_exec_core.raw_fields`.
/// Returns `true` when the candidate should be skipped.
pub(crate) async fn skip_by_filter(
    pool: &PgPool,
    partition_key: i16,
    exec_uuid: uuid::Uuid,
    filter: &ScannerFilter,
) -> bool {
    if filter.is_noop() {
        return false;
    }
    let row = sqlx::query(
        r#"
        SELECT raw_fields->>'namespace' AS ns,
               raw_fields->'tags'       AS tags
          FROM ff_exec_core
         WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(partition_key)
    .bind(exec_uuid)
    .fetch_optional(pool)
    .await;
    let Ok(Some(row)) = row else {
        return true;
    };
    if let Some(ref want_ns) = filter.namespace {
        let got: Option<String> = row.try_get("ns").ok().flatten();
        if got.as_deref() != Some(want_ns.as_str()) {
            return true;
        }
    }
    if let Some((ref k, ref v)) = filter.instance_tag {
        let tags: Option<JsonValue> = row.try_get("tags").ok();
        let got = tags
            .as_ref()
            .and_then(|t| t.get(k))
            .and_then(|t| t.as_str());
        if got != Some(v.as_str()) {
            return true;
        }
    }
    false
}
