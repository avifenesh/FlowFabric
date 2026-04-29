//! Postgres `pending_wp_expiry` reconciler (RFC-020 Wave 9 minimal).
//!
//! Mirrors [`ff_engine::scanner::pending_wp_expiry`] — closes pending
//! waitpoints whose `expires_at_ms` has lapsed. The Valkey side scans
//! `ff:idx:{p:N}:pending_waitpoint_expiry` ZSET and fires
//! `FCALL ff_close_waitpoint` with ARGV `[waitpoint_id,
//! "never_committed"]`; on Postgres the authority is
//! `ff_waitpoint_pending.expires_at_ms` (partial index
//! `ff_waitpoint_pending_expires_idx`).
//!
//! Action (per-row tx):
//!   1. Re-check the waitpoint still exists and its deadline is
//!      expired (`FOR UPDATE` on `ff_waitpoint_pending`).
//!   2. Read the owning `(execution_id, waitpoint_key)` before delete.
//!   3. DELETE the `ff_waitpoint_pending` row — the Valkey semantic
//!      is "close" (one-shot; once expired it never receives its
//!      signal).
//!   4. Append a synthetic `never_committed` entry into
//!      `ff_suspension_current.member_map[waitpoint_key]` so the
//!      composite-condition evaluator sees the failure on next
//!      resume. Matches the Valkey Lua `ff_close_waitpoint` argv
//!      `[_, "never_committed"]` semantic.
//!
//! The suspended execution's own `timeout_at_ms` (on
//! `ff_suspension_current`) continues to be managed by the
//! `suspension_timeout` reconciler; this reconciler only closes the
//! waitpoint and records the failure marker. A suspension waiting on
//! exactly one waitpoint and that waitpoint expiring will be woken
//! by the separate `suspension_timeout` scanner when its own deadline
//! elapses — which in practice is pinned to the same or later
//! deadline as the waitpoint's.

use ff_core::engine_error::EngineError;
use serde_json::{Value as JsonValue, json};
use sqlx::{PgPool, Row};

use crate::error::map_sqlx_error;

/// Per-waitpoint close action. Exposed so the
/// `EngineBackend::close_waitpoint` trait dispatch (PR-7b Cluster 1)
/// can invoke the same per-row tx logic the batch scanner would.
/// Silently no-ops when the waitpoint no longer exists or its
/// deadline has not yet elapsed (re-checked under `FOR UPDATE`).
pub async fn close_for_execution(
    pool: &PgPool,
    partition_key: i16,
    waitpoint_uuid: uuid::Uuid,
    now_ms: i64,
) -> Result<(), EngineError> {
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    let row = sqlx::query(
        r#"
        SELECT execution_id, waitpoint_key, expires_at_ms
          FROM ff_waitpoint_pending
         WHERE partition_key = $1 AND waitpoint_id = $2
         FOR UPDATE
        "#,
    )
    .bind(partition_key)
    .bind(waitpoint_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(row) = row else {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(());
    };
    let exec_uuid: uuid::Uuid = row.try_get("execution_id").map_err(map_sqlx_error)?;
    let waitpoint_key: String = row.try_get("waitpoint_key").map_err(map_sqlx_error)?;
    let expires_at: Option<i64> = row
        .try_get::<Option<i64>, _>("expires_at_ms")
        .map_err(map_sqlx_error)?;

    // Guard: if there's no expires_at (never meant to expire) or it
    // hasn't elapsed, no-op. The scanner side already filters by
    // `expires_at_ms < now` but the re-check under FOR UPDATE
    // protects against races with deliver_signal resolving the
    // waitpoint between scan + action.
    if !matches!(expires_at, Some(t) if t < now_ms) {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(());
    }

    // Delete the pending row — mirrors the Valkey `DEL
    // ff:wp:{tag}:<wp_id>` + `ZREM pending_waitpoint_expiry` pair.
    sqlx::query(
        r#"
        DELETE FROM ff_waitpoint_pending
         WHERE partition_key = $1 AND waitpoint_id = $2
        "#,
    )
    .bind(partition_key)
    .bind(waitpoint_uuid)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // Record `never_committed` marker on the owning suspension, if
    // any. Suspensions waiting on this waitpoint read member_map at
    // resume time; the marker lets the composite-condition evaluator
    // distinguish "signal delivered" from "waitpoint expired without
    // signal". Silent no-op when no suspension exists (the exec may
    // have resumed through another path already).
    let susp_row = sqlx::query(
        r#"
        SELECT member_map
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

    if let Some(susp_row) = susp_row {
        let member_map: JsonValue = susp_row.try_get("member_map").map_err(map_sqlx_error)?;
        let mut map = match member_map {
            JsonValue::Object(m) => m,
            _ => serde_json::Map::new(),
        };
        // Key on the waitpoint_key (human-readable) per RFC-013/014
        // suspend-condition contract; the Valkey Lua closes on the
        // same identifier.
        let key = if waitpoint_key.is_empty() {
            waitpoint_uuid.to_string()
        } else {
            waitpoint_key
        };
        map.insert(
            key,
            json!({
                "status": "never_committed",
                "closed_at_ms": now_ms,
                "source": "pending_wp_expiry",
            }),
        );
        sqlx::query(
            r#"
            UPDATE ff_suspension_current
               SET member_map = $1
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

    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(())
}
